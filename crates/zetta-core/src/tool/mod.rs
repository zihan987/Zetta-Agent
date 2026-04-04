mod bash;
mod echo;
mod file_edit;
mod file_edit_lines;
mod file_read;
mod file_read_lines;
mod file_write;
mod glob;
mod grep;
mod permission;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use zetta_protocol::{ToolCall, ToolResult};

pub use bash::BashTool;
pub use echo::EchoTool;
pub use file_edit::FileEditTool;
pub use file_edit_lines::FileEditLinesTool;
pub use file_read::FileReadTool;
pub use file_read_lines::FileReadLinesTool;
pub use file_write::FileWriteTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use permission::{PermissionMode, PermissionPolicy, PermissionRules};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCapability {
    Read,
    Write,
    Execute,
    Safe,
}

impl ToolCapability {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Safe => "safe",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub capability: ToolCapability,
}

#[derive(Clone, Debug)]
pub struct ToolUseContext {
    cwd: PathBuf,
    permissions: PermissionPolicy,
}

impl ToolUseContext {
    pub fn new(cwd: impl AsRef<Path>, permissions: PermissionPolicy) -> Result<Self> {
        Ok(Self {
            cwd: std::fs::canonicalize(cwd.as_ref())?,
            permissions,
        })
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn permissions(&self) -> &PermissionPolicy {
        &self.permissions
    }
}

#[derive(Debug)]
pub enum ToolInvocationError {
    Denied { reason: String },
    Failed(anyhow::Error),
}

impl std::fmt::Display for ToolInvocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied { reason } => write!(f, "{reason}"),
            Self::Failed(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ToolInvocationError {}

impl From<anyhow::Error> for ToolInvocationError {
    fn from(value: anyhow::Error) -> Self {
        Self::Failed(value)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn capability(&self) -> ToolCapability;
    async fn invoke(
        &self,
        input: Value,
        context: &ToolUseContext,
    ) -> Result<Value, ToolInvocationError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    pub async fn invoke(
        &self,
        call: &ToolCall,
        context: &ToolUseContext,
    ) -> Result<ToolResult, ToolInvocationError> {
        let tool = self.tools.get(&call.name).ok_or_else(|| {
            ToolInvocationError::Failed(anyhow!("tool `{}` is not registered", call.name))
        })?;
        context
            .permissions()
            .check_tool_allowed(tool.name(), tool.capability())?;

        let output = tool.invoke(call.input.clone(), context).await?;
        Ok(ToolResult {
            name: call.name.clone(),
            output,
        })
    }

    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        let mut names = self
            .tools
            .values()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    }

    #[must_use]
    pub fn visible_names<'a>(&'a self, context: &ToolUseContext) -> Vec<&'a str> {
        let mut names = self
            .tools
            .values()
            .filter(|tool| {
                context
                    .permissions()
                    .is_tool_visible(tool.name(), tool.capability())
            })
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    }

    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                capability: tool.capability(),
            })
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    #[must_use]
    pub fn visible_definitions(&self, context: &ToolUseContext) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .values()
            .filter(|tool| {
                context
                    .permissions()
                    .is_tool_visible(tool.name(), tool.capability())
            })
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                capability: tool.capability(),
            })
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;
    use zetta_protocol::ToolCall;

    use super::{
        BashTool, EchoTool, FileEditLinesTool, FileEditTool, FileReadLinesTool, FileReadTool,
        FileWriteTool, GlobTool, GrepTool, PermissionMode, PermissionPolicy, PermissionRules,
        ToolInvocationError, ToolRegistry, ToolUseContext,
    };

    fn build_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        registry.register(EchoTool);
        registry.register(BashTool);
        registry.register(FileReadTool);
        registry.register(FileReadLinesTool);
        registry.register(FileEditTool);
        registry.register(FileEditLinesTool);
        registry.register(FileWriteTool);
        registry.register(GlobTool);
        registry.register(GrepTool);
        registry
    }

    #[tokio::test]
    async fn file_write_and_read_work_inside_workspace() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        registry
            .invoke(
                &ToolCall {
                    name: "file_write".to_string(),
                    input: json!({
                        "path": "notes/hello.txt",
                        "content": "rust rewrite"
                    }),
                },
                &context,
            )
            .await?;

        let result = registry
            .invoke(
                &ToolCall {
                    name: "file_read".to_string(),
                    input: json!({
                        "path": "notes/hello.txt"
                    }),
                },
                &context,
            )
            .await?;

        assert_eq!(result.output["content"], "rust rewrite");
        Ok(())
    }

    #[tokio::test]
    async fn file_edit_replaces_exact_match() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::write(temp_dir.path().join("notes.txt"), "alpha\nbeta\ngamma\n")?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let result = registry
            .invoke(
                &ToolCall {
                    name: "file_edit".to_string(),
                    input: json!({
                        "path": "notes.txt",
                        "old_text": "beta",
                        "new_text": "delta"
                    }),
                },
                &context,
            )
            .await?;

        assert_eq!(result.output["replacement_count"], 1);
        assert_eq!(result.output["operation"], "replace");
        assert_eq!(result.output["first_replacement"]["start_line"], 2);
        assert_eq!(result.output["first_replacement"]["end_line"], 2);
        assert!(result.output["first_replacement"]["before_preview"]
            .as_str()
            .unwrap_or_default()
            .contains("beta"));
        assert!(result.output["first_replacement"]["after_preview"]
            .as_str()
            .unwrap_or_default()
            .contains("delta"));
        let contents = std::fs::read_to_string(temp_dir.path().join("notes.txt"))?;
        assert_eq!(contents, "alpha\ndelta\ngamma\n");
        Ok(())
    }

    #[tokio::test]
    async fn file_edit_requires_specific_match_unless_replace_all() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::write(temp_dir.path().join("dup.txt"), "same\nsame\n")?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let error = registry
            .invoke(
                &ToolCall {
                    name: "file_edit".to_string(),
                    input: json!({
                        "path": "dup.txt",
                        "old_text": "same",
                        "new_text": "changed"
                    }),
                },
                &context,
            )
            .await
            .expect_err("ambiguous file_edit should fail");

        assert!(error.to_string().contains("replace_all=true"));

        let result = registry
            .invoke(
                &ToolCall {
                    name: "file_edit".to_string(),
                    input: json!({
                        "path": "dup.txt",
                        "old_text": "same",
                        "new_text": "changed",
                        "replace_all": true
                    }),
                },
                &context,
            )
            .await?;

        assert_eq!(result.output["replacement_count"], 2);
        assert_eq!(result.output["operation"], "replace_all");
        assert_eq!(result.output["first_replacement"]["start_line"], 1);
        assert_eq!(result.output["last_replacement"]["start_line"], 2);
        let contents = std::fs::read_to_string(temp_dir.path().join("dup.txt"))?;
        assert_eq!(contents, "changed\nchanged\n");
        Ok(())
    }

    #[tokio::test]
    async fn file_edit_lines_replaces_inclusive_range() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::write(temp_dir.path().join("notes.txt"), "one\ntwo\nthree\nfour\n")?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let result = registry
            .invoke(
                &ToolCall {
                    name: "file_edit_lines".to_string(),
                    input: json!({
                        "path": "notes.txt",
                        "start_line": 2,
                        "end_line": 3,
                        "new_text": "delta\nepsilon"
                    }),
                },
                &context,
            )
            .await?;

        assert_eq!(result.output["replaced_line_count"], 2);
        assert_eq!(result.output["inserted_line_count"], 2);
        assert_eq!(result.output["operation"], "replace");
        assert_eq!(result.output["before_preview"], "two\nthree");
        assert_eq!(result.output["after_preview"], "delta\nepsilon");
        let contents = std::fs::read_to_string(temp_dir.path().join("notes.txt"))?;
        assert_eq!(contents, "one\ndelta\nepsilon\nfour\n");
        Ok(())
    }

    #[tokio::test]
    async fn file_edit_lines_allows_deleting_range() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::write(temp_dir.path().join("notes.txt"), "one\ntwo\nthree\n")?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let result = registry
            .invoke(
                &ToolCall {
                    name: "file_edit_lines".to_string(),
                    input: json!({
                        "path": "notes.txt",
                        "start_line": 2,
                        "end_line": 3,
                        "new_text": ""
                    }),
                },
                &context,
            )
            .await?;

        assert_eq!(result.output["operation"], "delete");
        assert_eq!(result.output["before_preview"], "two\nthree");
        assert_eq!(result.output["after_preview"], "");
        let contents = std::fs::read_to_string(temp_dir.path().join("notes.txt"))?;
        assert_eq!(contents, "one\n");
        Ok(())
    }

    #[tokio::test]
    async fn file_read_lines_returns_inclusive_range_with_numbers() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::write(temp_dir.path().join("lines.txt"), "one\ntwo\nthree\nfour\n")?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let result = registry
            .invoke(
                &ToolCall {
                    name: "file_read_lines".to_string(),
                    input: json!({
                        "path": "lines.txt",
                        "start_line": 2,
                        "end_line": 3
                    }),
                },
                &context,
            )
            .await?;

        assert_eq!(result.output["content"], "two\nthree");
        assert_eq!(result.output["lines"][0]["line_number"], 2);
        assert_eq!(result.output["lines"][1]["text"], "three");
        Ok(())
    }

    #[tokio::test]
    async fn file_read_lines_clamps_end_but_rejects_start_past_eof() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::write(temp_dir.path().join("lines.txt"), "one\ntwo\n")?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let result = registry
            .invoke(
                &ToolCall {
                    name: "file_read_lines".to_string(),
                    input: json!({
                        "path": "lines.txt",
                        "start_line": 2,
                        "end_line": 9
                    }),
                },
                &context,
            )
            .await?;
        assert_eq!(result.output["end_line"], 2);

        let error = registry
            .invoke(
                &ToolCall {
                    name: "file_read_lines".to_string(),
                    input: json!({
                        "path": "lines.txt",
                        "start_line": 5,
                        "end_line": 9
                    }),
                },
                &context,
            )
            .await
            .expect_err("start past eof should fail");
        assert!(error.to_string().contains("past end of file"));
        Ok(())
    }

    #[tokio::test]
    async fn shell_is_denied_in_read_only_mode() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::ReadOnly,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let error = registry
            .invoke(
                &ToolCall {
                    name: "bash".to_string(),
                    input: json!({
                        "command": "pwd"
                    }),
                },
                &context,
            )
            .await
            .expect_err("bash should be denied");

        match error {
            ToolInvocationError::Denied { reason } => {
                assert!(reason.contains("read-only"));
            }
            ToolInvocationError::Failed(error) => {
                panic!("expected denial, got execution error: {error}");
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn shell_safe_command_runs_in_workspace_write_mode() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let result = registry
            .invoke(
                &ToolCall {
                    name: "bash".to_string(),
                    input: json!({
                        "command": "pwd"
                    }),
                },
                &context,
            )
            .await?;

        assert_eq!(result.output["success"], true);
        assert!(result.output["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains(temp_dir.path().to_string_lossy().as_ref()));
        Ok(())
    }

    #[tokio::test]
    async fn shell_blocks_high_risk_executables() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let error = registry
            .invoke(
                &ToolCall {
                    name: "bash".to_string(),
                    input: json!({
                        "command": "rm -rf scratch.txt"
                    }),
                },
                &context,
            )
            .await
            .expect_err("rm should be denied");

        match error {
            ToolInvocationError::Denied { reason } => {
                assert!(reason.contains("blocked"));
                assert!(reason.contains("rm"));
            }
            ToolInvocationError::Failed(error) => {
                panic!("expected denial, got execution error: {error}");
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn shell_blocks_redirection_and_command_chaining() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let chained_error = registry
            .invoke(
                &ToolCall {
                    name: "bash".to_string(),
                    input: json!({
                        "command": "git status && cargo test"
                    }),
                },
                &context,
            )
            .await
            .expect_err("chained command should be denied");
        assert!(chained_error.to_string().contains("chaining"));

        let redirect_error = registry
            .invoke(
                &ToolCall {
                    name: "bash".to_string(),
                    input: json!({
                        "command": "echo hi > note.txt"
                    }),
                },
                &context,
            )
            .await
            .expect_err("redirection should be denied");
        assert!(redirect_error.to_string().contains("redirection"));

        Ok(())
    }

    #[tokio::test]
    async fn shell_restrictions_are_skipped_in_bypass_mode() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::BypassPermissions,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        registry
            .invoke(
                &ToolCall {
                    name: "bash".to_string(),
                    input: json!({
                        "command": "printf hi > bypass.txt"
                    }),
                },
                &context,
            )
            .await?;

        assert_eq!(
            std::fs::read_to_string(temp_dir.path().join("bypass.txt"))?,
            "hi"
        );
        Ok(())
    }

    #[tokio::test]
    async fn file_read_denies_sensitive_secret_files() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::write(temp_dir.path().join(".env"), "API_KEY=secret\n")?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let error = registry
            .invoke(
                &ToolCall {
                    name: "file_read".to_string(),
                    input: json!({
                        "path": ".env"
                    }),
                },
                &context,
            )
            .await
            .expect_err("sensitive env file should be denied");

        assert!(error.to_string().contains("protected"));
        assert!(error.to_string().contains(".env"));
        Ok(())
    }

    #[tokio::test]
    async fn file_write_denies_repository_metadata_and_runtime_state() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::create_dir_all(temp_dir.path().join(".git"))?;
        std::fs::create_dir_all(temp_dir.path().join(".zetta"))?;
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let git_error = registry
            .invoke(
                &ToolCall {
                    name: "file_write".to_string(),
                    input: json!({
                        "path": ".git/config",
                        "content": "blocked"
                    }),
                },
                &context,
            )
            .await
            .expect_err(".git writes should be denied");
        assert!(git_error.to_string().contains(".git/config"));

        let runtime_error = registry
            .invoke(
                &ToolCall {
                    name: "file_write".to_string(),
                    input: json!({
                        "path": ".zetta/session.json",
                        "content": "blocked"
                    }),
                },
                &context,
            )
            .await
            .expect_err(".zetta writes should be denied");
        assert!(runtime_error.to_string().contains(".zetta/session.json"));

        Ok(())
    }

    #[tokio::test]
    async fn file_write_denies_existing_symlink_targets() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let real_path = temp_dir.path().join("real.txt");
        let link_path = temp_dir.path().join("linked.txt");
        std::fs::write(&real_path, "original")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_path, &link_path)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&real_path, &link_path)?;

        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let error = registry
            .invoke(
                &ToolCall {
                    name: "file_write".to_string(),
                    input: json!({
                        "path": "linked.txt",
                        "content": "updated"
                    }),
                },
                &context,
            )
            .await
            .expect_err("symlink writes should be denied");

        assert!(error.to_string().contains("symlink"));
        assert_eq!(std::fs::read_to_string(real_path)?, "original");
        Ok(())
    }

    #[tokio::test]
    async fn grep_and_glob_find_workspace_files() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::create_dir_all(temp_dir.path().join("src"))?;
        std::fs::write(
            temp_dir.path().join("src/lib.rs"),
            "fn main() {\n    println!(\"needle\");\n}\n",
        )?;
        std::fs::write(
            temp_dir.path().join("src/notes.txt"),
            "needle in a text file\n",
        )?;

        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let glob_result = registry
            .invoke(
                &ToolCall {
                    name: "glob".to_string(),
                    input: json!({
                        "pattern": "src/*.rs"
                    }),
                },
                &context,
            )
            .await?;
        let grep_result = registry
            .invoke(
                &ToolCall {
                    name: "grep".to_string(),
                    input: json!({
                        "pattern": "needle"
                    }),
                },
                &context,
            )
            .await?;

        assert_eq!(glob_result.output["matches"][0], "src/lib.rs");
        assert_eq!(grep_result.output["matches"].as_array().unwrap().len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn grep_and_glob_skip_sensitive_entries() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        std::fs::create_dir_all(temp_dir.path().join(".git"))?;
        std::fs::write(temp_dir.path().join(".git/secret.txt"), "needle\n")?;
        std::fs::write(temp_dir.path().join(".env"), "needle=1\n")?;
        std::fs::write(temp_dir.path().join("visible.txt"), "needle visible\n")?;

        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let registry = build_registry();

        let glob_result = registry
            .invoke(
                &ToolCall {
                    name: "glob".to_string(),
                    input: json!({
                        "pattern": "*"
                    }),
                },
                &context,
            )
            .await?;
        let grep_result = registry
            .invoke(
                &ToolCall {
                    name: "grep".to_string(),
                    input: json!({
                        "pattern": "needle"
                    }),
                },
                &context,
            )
            .await?;

        let glob_matches = glob_result.output["matches"]
            .as_array()
            .expect("glob matches");
        assert_eq!(glob_matches, &vec![json!("visible.txt")]);

        let grep_matches = grep_result.output["matches"]
            .as_array()
            .expect("grep matches");
        assert_eq!(grep_matches.len(), 1);
        assert_eq!(grep_matches[0]["path"], "visible.txt");
        Ok(())
    }

    #[tokio::test]
    async fn explicit_tool_deny_rule_hides_and_blocks_tool() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let mut rules = PermissionRules::default();
        rules.denied_tools.insert("bash".to_string());
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite, temp_dir.path(), rules)?,
        )?;
        let registry = build_registry();

        assert!(!registry.visible_names(&context).contains(&"bash"));

        let error = registry
            .invoke(
                &ToolCall {
                    name: "bash".to_string(),
                    input: json!({
                        "command": "pwd"
                    }),
                },
                &context,
            )
            .await
            .expect_err("bash should be denied");

        match error {
            ToolInvocationError::Denied { reason } => {
                assert!(reason.contains("denied by policy"));
            }
            ToolInvocationError::Failed(error) => {
                panic!("expected denial, got execution error: {error}");
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn allow_list_restricts_visible_tools() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let mut rules = PermissionRules::default();
        rules.allowed_tools.insert("file_read".to_string());
        rules.allowed_tools.insert("glob".to_string());
        let context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite, temp_dir.path(), rules)?,
        )?;
        let registry = build_registry();

        let visible = registry.visible_names(&context);
        assert_eq!(visible, vec!["file_read", "glob"]);
        Ok(())
    }
}
