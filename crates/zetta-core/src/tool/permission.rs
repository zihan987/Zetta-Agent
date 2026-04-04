use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use super::{ToolCapability, ToolInvocationError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    ReadOnly,
    WorkspaceWrite,
    BypassPermissions,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PermissionRules {
    pub readable_roots: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    pub allowed_tools: HashSet<String>,
    pub denied_tools: HashSet<String>,
}

#[derive(Clone, Debug)]
pub struct PermissionPolicy {
    mode: PermissionMode,
    workspace_root: PathBuf,
    readable_roots: Vec<PathBuf>,
    writable_roots: Vec<PathBuf>,
    allowed_tools: HashSet<String>,
    denied_tools: HashSet<String>,
}

impl PermissionPolicy {
    pub fn new(
        mode: PermissionMode,
        workspace_root: impl AsRef<Path>,
        rules: PermissionRules,
    ) -> Result<Self> {
        let workspace_root = std::fs::canonicalize(workspace_root.as_ref())?;
        let mut readable_roots = resolve_roots(rules.readable_roots)?;
        let mut writable_roots = resolve_roots(rules.writable_roots)?;

        if readable_roots.is_empty() {
            readable_roots.push(workspace_root.clone());
        }

        if writable_roots.is_empty() {
            writable_roots.push(workspace_root.clone());
        }

        Ok(Self {
            mode,
            workspace_root,
            readable_roots,
            writable_roots,
            allowed_tools: rules.allowed_tools,
            denied_tools: rules.denied_tools,
        })
    }

    pub fn check_tool_allowed(
        &self,
        tool_name: &str,
        capability: ToolCapability,
    ) -> Result<(), ToolInvocationError> {
        if self.mode != PermissionMode::BypassPermissions && self.denied_tools.contains(tool_name) {
            return Err(ToolInvocationError::Denied {
                reason: format!("tool `{tool_name}` is denied by policy"),
            });
        }

        if self.mode != PermissionMode::BypassPermissions
            && !self.allowed_tools.is_empty()
            && !self.allowed_tools.contains(tool_name)
        {
            return Err(ToolInvocationError::Denied {
                reason: format!("tool `{tool_name}` is not in the allow list"),
            });
        }

        if self.mode == PermissionMode::ReadOnly
            && matches!(capability, ToolCapability::Write | ToolCapability::Execute)
        {
            return Err(ToolInvocationError::Denied {
                reason: format!("tool `{tool_name}` is disabled in read-only mode"),
            });
        }

        Ok(())
    }

    #[must_use]
    pub fn is_tool_visible(&self, tool_name: &str, capability: ToolCapability) -> bool {
        self.check_tool_allowed(tool_name, capability).is_ok()
    }

    pub fn resolve_read_path(
        &self,
        cwd: &Path,
        raw_path: &str,
    ) -> Result<PathBuf, ToolInvocationError> {
        let resolved = resolve_input_path(cwd, raw_path);
        let canonical = std::fs::canonicalize(&resolved).map_err(|error| {
            ToolInvocationError::Failed(anyhow!(
                "failed to resolve read path `{raw_path}`: {error}"
            ))
        })?;

        if self.mode != PermissionMode::BypassPermissions
            && !self
                .readable_roots
                .iter()
                .any(|root| is_within(&canonical, root))
        {
            return Err(ToolInvocationError::Denied {
                reason: format!(
                    "read path `{}` is outside readable roots under workspace `{}`",
                    canonical.display(),
                    self.workspace_root.display()
                ),
            });
        }

        self.check_sensitive_read_path(&canonical)?;
        Ok(canonical)
    }

    pub fn resolve_write_path(
        &self,
        cwd: &Path,
        raw_path: &str,
    ) -> Result<PathBuf, ToolInvocationError> {
        let resolved = resolve_input_path(cwd, raw_path);
        self.check_sensitive_write_path(&resolved)?;

        if self.mode == PermissionMode::BypassPermissions {
            return Ok(resolved);
        }

        if resolved.exists() {
            let metadata = std::fs::symlink_metadata(&resolved).map_err(|error| {
                ToolInvocationError::Failed(anyhow!(
                    "failed to inspect write target `{}`: {error}",
                    resolved.display()
                ))
            })?;

            if metadata.file_type().is_symlink() {
                return Err(ToolInvocationError::Denied {
                    reason: format!(
                        "write path `{}` is a symlink; writing through symlinks is not allowed",
                        resolved.display()
                    ),
                });
            }

            if metadata.is_dir() {
                return Err(ToolInvocationError::Failed(anyhow!(
                    "write path `{}` is a directory, not a file",
                    resolved.display()
                )));
            }

            let canonical_target = std::fs::canonicalize(&resolved).map_err(|error| {
                ToolInvocationError::Failed(anyhow!(
                    "failed to resolve write target `{}`: {error}",
                    resolved.display()
                ))
            })?;

            if !self
                .writable_roots
                .iter()
                .any(|root| is_within(&canonical_target, root))
            {
                return Err(ToolInvocationError::Denied {
                    reason: format!(
                        "write path `{}` resolves outside writable roots",
                        canonical_target.display()
                    ),
                });
            }
        }

        let existing_ancestor = nearest_existing_ancestor(&resolved).ok_or_else(|| {
            ToolInvocationError::Failed(anyhow!(
                "could not find an existing parent for write path `{}`",
                resolved.display()
            ))
        })?;

        let canonical_ancestor = std::fs::canonicalize(&existing_ancestor).map_err(|error| {
            ToolInvocationError::Failed(anyhow!(
                "failed to resolve parent directory `{}`: {error}",
                existing_ancestor.display()
            ))
        })?;

        let allowed = self
            .writable_roots
            .iter()
            .any(|root| is_within(&canonical_ancestor, root));

        if !allowed {
            return Err(ToolInvocationError::Denied {
                reason: format!(
                    "write path `{}` is outside writable roots",
                    resolved.display()
                ),
            });
        }

        Ok(resolved)
    }

    #[must_use]
    pub fn should_skip_walk_entry_for_read(&self, path: &Path) -> bool {
        self.mode != PermissionMode::BypassPermissions && sensitive_read_reason(path).is_some()
    }

    pub fn check_shell_command(&self, command: &str) -> Result<(), ToolInvocationError> {
        if self.mode == PermissionMode::BypassPermissions {
            return Ok(());
        }

        let trimmed = command.trim();
        if trimmed.is_empty() {
            return Err(ToolInvocationError::Denied {
                reason: "bash command cannot be empty".to_string(),
            });
        }

        if let Some(reason) = detect_disallowed_shell_construct(trimmed) {
            return Err(ToolInvocationError::Denied { reason });
        }

        let executable = trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .rsplit_once('/')
            .map(|(_, leaf)| leaf)
            .unwrap_or_else(|| trimmed.split_whitespace().next().unwrap_or_default())
            .to_ascii_lowercase();

        if executable.contains('=') {
            return Err(ToolInvocationError::Denied {
                reason: "inline environment assignments are not allowed in `bash` commands"
                    .to_string(),
            });
        }

        if is_disallowed_executable(&executable) {
            return Err(ToolInvocationError::Denied {
                reason: format!(
                    "bash command `{executable}` is blocked in permission mode `{:?}`",
                    self.mode
                ),
            });
        }

        Ok(())
    }

    fn check_sensitive_read_path(&self, path: &Path) -> Result<(), ToolInvocationError> {
        if self.mode == PermissionMode::BypassPermissions {
            return Ok(());
        }

        if let Some(reason) = sensitive_read_reason(path) {
            return Err(ToolInvocationError::Denied {
                reason: format!("read path `{}` is protected: {reason}", path.display()),
            });
        }

        Ok(())
    }

    fn check_sensitive_write_path(&self, path: &Path) -> Result<(), ToolInvocationError> {
        if self.mode == PermissionMode::BypassPermissions {
            return Ok(());
        }

        if let Some(reason) = sensitive_write_reason(path) {
            return Err(ToolInvocationError::Denied {
                reason: format!("write path `{}` is protected: {reason}", path.display()),
            });
        }

        Ok(())
    }
}

fn resolve_roots(roots: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    roots
        .into_iter()
        .map(|root| std::fs::canonicalize(root).map_err(anyhow::Error::from))
        .collect()
}

fn resolve_input_path(cwd: &Path, raw_path: &str) -> PathBuf {
    let path = Path::new(raw_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cursor = Some(path);

    while let Some(candidate) = cursor {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        cursor = candidate.parent();
    }

    None
}

fn is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn sensitive_read_reason(path: &Path) -> Option<&'static str> {
    if contains_sensitive_dir_component(path) {
        return Some("repository internals or secret directories are hidden from file tools");
    }

    let file_name = path.file_name().and_then(|value| value.to_str())?;
    if is_sensitive_secret_file(file_name) {
        return Some("secret environment or credential files require manual review");
    }

    None
}

fn sensitive_write_reason(path: &Path) -> Option<&'static str> {
    if contains_protected_write_dir_component(path) {
        return Some("protected runtime or repository metadata cannot be modified by file tools");
    }

    let file_name = path.file_name().and_then(|value| value.to_str())?;
    if is_sensitive_secret_file(file_name) {
        return Some("secret environment or credential files cannot be modified by file tools");
    }

    None
}

fn contains_sensitive_dir_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if matches!(
                    name.to_str(),
                    Some(".git" | ".ssh" | ".gnupg" | ".aws" | ".azure")
                )
        )
    })
}

fn contains_protected_write_dir_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if matches!(
                    name.to_str(),
                    Some(".git" | ".ssh" | ".gnupg" | ".aws" | ".azure" | ".zetta")
                )
        )
    })
}

fn is_sensitive_secret_file(file_name: &str) -> bool {
    matches!(
        file_name,
        ".env" | ".npmrc" | ".pypirc" | ".netrc" | "id_rsa" | "id_ed25519"
    ) || (file_name.starts_with(".env.")
        && !matches!(
            file_name,
            ".env.example" | ".env.sample" | ".env.template" | ".env.test.example"
        ))
}

fn detect_disallowed_shell_construct(command: &str) -> Option<String> {
    let forbidden = [
        ("&&", "command chaining with `&&` is not allowed"),
        ("||", "command chaining with `||` is not allowed"),
        (
            ";",
            "multiple shell commands separated by `;` are not allowed",
        ),
        ("\n", "multi-line shell commands are not allowed"),
        ("$(", "subshell command substitution is not allowed"),
        ("`", "backtick command substitution is not allowed"),
        (" | ", "shell pipelines are not allowed"),
        ("|&", "shell pipelines are not allowed"),
        (" >", "shell redirection is not allowed"),
        (" >>", "shell redirection is not allowed"),
        (" 2>", "shell redirection is not allowed"),
        (" 1>", "shell redirection is not allowed"),
        (" < ", "input redirection is not allowed"),
    ];

    forbidden
        .into_iter()
        .find(|(pattern, _)| command.contains(pattern))
        .map(|(_, reason)| reason.to_string())
}

fn is_disallowed_executable(executable: &str) -> bool {
    matches!(
        executable,
        "bash"
            | "sh"
            | "zsh"
            | "fish"
            | "sudo"
            | "su"
            | "rm"
            | "mv"
            | "chmod"
            | "chown"
            | "dd"
            | "mkfs"
            | "fdisk"
            | "diskutil"
            | "mount"
            | "umount"
            | "shutdown"
            | "reboot"
            | "halt"
            | "poweroff"
            | "kill"
            | "killall"
            | "pkill"
            | "launchctl"
            | "systemctl"
            | "service"
            | "curl"
            | "wget"
            | "scp"
            | "ssh"
            | "nc"
            | "ncat"
            | "telnet"
            | "python"
            | "python3"
            | "node"
            | "perl"
            | "ruby"
            | "php"
            | "osascript"
            | "open"
            | "xdg-open"
    )
}
