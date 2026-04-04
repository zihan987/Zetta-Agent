mod hook_config;
mod permission_config;

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Error, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use hook_config::{merge_hook_configs, HookConfigStore, HookScope, PersistentHookConfig};
use permission_config::{
    merge_permission_configs, PermissionConfigStore, PermissionScope, PersistentPermissionConfig,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use zetta_core::engine::AgentEngine;
use zetta_core::hook::{DenyToolHook, HookBus, JsonlHook, SessionAnnotatingHook};
use zetta_core::model::{
    tool_call_from_user_input, ModelClient, ModelStreamSink, OpenAiCompatibleConfig,
    OpenAiCompatibleModelClient, RuleBasedModelClient,
};
use zetta_core::session::{FileSessionStore, SessionStore};
use zetta_core::tool::{
    BashTool, EchoTool, FileEditLinesTool, FileEditTool, FileReadLinesTool, FileReadTool,
    FileWriteTool, GlobTool, GrepTool, PermissionMode, PermissionPolicy, PermissionRules,
    ToolDefinition, ToolInvocationError, ToolRegistry, ToolUseContext,
};
use zetta_protocol::{SessionId, ToolCall, TurnRequest};

const PROJECT_CONFIG_DIRNAME: &str = ".zetta";
const PROJECT_PERMISSION_CONFIG_FILENAME: &str = "project-permissions.json";
const PROJECT_HOOK_CONFIG_FILENAME: &str = "project-hooks.json";

#[derive(Parser)]
#[command(name = "zetta")]
#[command(about = "Headless Rust agent runtime for Zetta")]
struct Cli {
    #[arg(long, global = true, default_value = ".zetta")]
    config_dir: PathBuf,

    #[arg(long, global = true, default_value = ".zetta/sessions")]
    session_dir: PathBuf,

    #[arg(long, global = true)]
    workspace_root: Option<PathBuf>,

    #[arg(long, global = true, value_enum)]
    permission_mode: Option<CliPermissionMode>,

    #[arg(long, global = true)]
    readable_root: Vec<PathBuf>,

    #[arg(long, global = true)]
    writable_root: Vec<PathBuf>,

    #[arg(long, global = true)]
    allow_tool: Vec<String>,

    #[arg(long, global = true)]
    deny_tool: Vec<String>,

    #[arg(long, global = true)]
    hook_log: Option<PathBuf>,

    #[arg(long, global = true)]
    hook_deny_tool: Vec<String>,

    #[arg(long, global = true)]
    hook_tag: Vec<String>,

    #[arg(long, global = true)]
    hook_metadata: Vec<String>,

    #[arg(long, global = true, value_enum, default_value = "rule-based")]
    model_driver: CliModelDriver,

    #[arg(long, global = true)]
    model_name: Option<String>,

    #[arg(long, global = true)]
    api_base: Option<String>,

    #[arg(long, global = true, default_value = "OPENAI_API_KEY")]
    api_key_env: String,

    #[arg(long, global = true)]
    system_prompt: Option<String>,

    #[arg(long, global = true)]
    stream_output: bool,

    #[arg(long, global = true, default_value_t = 45)]
    request_timeout_seconds: u64,

    #[arg(long, global = true, default_value_t = 2)]
    max_model_retries: usize,

    #[arg(long, global = true, default_value_t = 500)]
    retry_backoff_millis: u64,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliPermissionMode {
    ReadOnly,
    WorkspaceWrite,
    BypassPermissions,
}

#[derive(Clone, Debug, ValueEnum)]
enum CliModelDriver {
    RuleBased,
    OpenaiCompatible,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        #[arg(long)]
        prompt: String,

        #[arg(long)]
        session_id: Option<SessionId>,

        #[arg(long)]
        json: bool,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },
    Tool {
        #[command(subcommand)]
        command: ToolCommands,
    },
    Permission {
        #[command(subcommand)]
        command: PermissionCommands,
    },
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },
}

#[derive(Subcommand)]
enum SessionCommands {
    Show {
        #[arg(long)]
        session_id: SessionId,
    },
}

#[derive(Subcommand)]
enum ToolCommands {
    List {
        #[arg(long)]
        session_id: Option<SessionId>,
    },
    Call {
        #[arg(long)]
        name: String,

        #[arg(long)]
        input: Option<String>,

        #[arg(long)]
        raw: Option<String>,

        #[arg(long)]
        session_id: Option<SessionId>,
    },
}

#[derive(Subcommand)]
enum PermissionCommands {
    Show(PermissionScopeArgs),
    Export {
        path: PathBuf,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    Import {
        path: PathBuf,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    SetMode {
        #[arg(value_enum)]
        mode: CliPermissionMode,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    AllowTool {
        name: String,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    DenyTool {
        name: String,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    AddReadableRoot {
        path: PathBuf,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    AddWritableRoot {
        path: PathBuf,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    Reset(PermissionScopeArgs),
}

#[derive(Subcommand)]
enum HookCommands {
    Show(HookScopeArgs),
    Export {
        path: PathBuf,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    Import {
        path: PathBuf,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    DenyTool {
        name: String,
        #[arg(long)]
        reason: Option<String>,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    AllowTool {
        name: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    AddTag {
        tag: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    RemoveTag {
        tag: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    SetMetadata {
        key: String,
        value: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    UnsetMetadata {
        key: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    Reset(HookScopeArgs),
}

#[derive(Args, Clone)]
struct PermissionScopeArgs {
    #[arg(long)]
    session_id: Option<SessionId>,
}

#[derive(Args, Clone)]
struct HookScopeArgs {
    #[arg(long)]
    session_id: Option<SessionId>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run_cli().await {
        print_cli_error(&error);
        process::exit(1);
    }
}

async fn run_cli() -> Result<()> {
    let cli = Cli::parse();

    let store = Arc::new(FileSessionStore::new(&cli.session_dir));
    let config_store = PermissionConfigStore::new(&cli.config_dir);
    let hook_config_store = HookConfigStore::new(&cli.config_dir);
    let cwd = env::current_dir()?;
    let workspace_root = cli.workspace_root.clone().unwrap_or_else(|| cwd.clone());
    let cli_overrides = cli_permission_overrides(&cli);
    let cli_hook_overrides = cli_hook_overrides(&cli);
    match cli.command {
        Commands::Run {
            ref prompt,
            session_id,
            json,
        } => {
            let hook_bus = build_hook_bus(
                cli.hook_log.as_ref(),
                &hook_config_store,
                &cli_hook_overrides,
                &workspace_root,
                session_id,
            )?;
            let tool_context = build_tool_context(
                &cli_overrides,
                &config_store,
                &cwd,
                &workspace_root,
                session_id,
            )?;
            let registry = build_registry();
            let model = build_model_client(&cli, registry.visible_definitions(&tool_context))?;
            let engine = AgentEngine::new(
                model.clone(),
                store.clone(),
                registry,
                tool_context,
                hook_bus,
            );
            let request = TurnRequest {
                session_id,
                prompt: prompt.clone(),
            };
            let output = if cli.stream_output {
                let mut sink = StderrModelStreamSink::default();
                engine
                    .run_turn_with_model_sink(request, Some(&mut sink))
                    .await?
            } else {
                engine.run_turn(request).await?
            };

            for failure in &output.hook_failures {
                eprintln!("hook `{}` failed: {}", failure.handler_name, failure.error);
            }

            if json {
                for event in output.events {
                    println!("{}", serde_json::to_string(&event)?);
                }
            } else {
                let assistant = output
                    .session
                    .messages
                    .iter()
                    .rev()
                    .find(|message| matches!(message.role, zetta_protocol::MessageRole::Assistant))
                    .map(|message| message.content.as_str())
                    .unwrap_or("<no assistant message>");

                println!("session_id: {}", output.session.session_id);
                println!("{assistant}");
            }
        }
        Commands::Session { command } => match command {
            SessionCommands::Show { session_id } => {
                let Some(session) = store.load(&session_id).await? else {
                    bail!("session `{session_id}` not found");
                };
                println!("{}", serde_json::to_string_pretty(&session)?);
            }
        },
        Commands::Tool { command } => match command {
            ToolCommands::List { session_id } => {
                let tool_context = build_tool_context(
                    &cli_overrides,
                    &config_store,
                    &cwd,
                    &workspace_root,
                    session_id,
                )?;
                for name in build_registry().visible_names(&tool_context) {
                    println!("{name}");
                }
            }
            ToolCommands::Call {
                name,
                input,
                raw,
                session_id,
            } => {
                let tool_context = build_tool_context(
                    &cli_overrides,
                    &config_store,
                    &cwd,
                    &workspace_root,
                    session_id,
                )?;
                let call = build_tool_call(name, input, raw)?;
                match build_registry().invoke(&call, &tool_context).await {
                    Ok(result) => println!("{}", serde_json::to_string_pretty(&result)?),
                    Err(ToolInvocationError::Denied { reason }) => {
                        bail!("tool denied: {reason}");
                    }
                    Err(ToolInvocationError::Failed(error)) => return Err(error),
                }
            }
        },
        Commands::Permission { command } => {
            handle_permission_command(command, &config_store)?;
        }
        Commands::Hook { command } => {
            handle_hook_command(command, &hook_config_store)?;
        }
    }

    Ok(())
}

fn print_cli_error(error: &Error) {
    for line in render_cli_error_lines(error) {
        eprintln!("{line}");
    }
}

fn render_cli_error_lines(error: &Error) -> Vec<String> {
    let detail = error.to_string();
    let lower = detail.to_ascii_lowercase();

    if detail.contains("requires env var `") {
        return vec![format!("Configuration error: {detail}")];
    }

    if lower.contains("timed out") || lower.contains("stalled") {
        return vec![
            format!("Model timeout: {detail}"),
            "Hint: increase `--request-timeout-seconds` or retry against a healthier provider endpoint.".to_string(),
        ];
    }

    if detail.contains("provider returned HTTP ") {
        let mut lines = vec![format!("Provider error: {detail}")];
        if detail.contains("HTTP 429") {
            lines.push(
                "Hint: rate limited; reduce concurrency, wait, or increase `--max-model-retries`."
                    .to_string(),
            );
        } else if detail.contains("HTTP 5") {
            lines.push(
                "Hint: upstream instability; retry later or increase `--max-model-retries`."
                    .to_string(),
            );
        } else if detail.contains("HTTP 401") || detail.contains("HTTP 403") {
            lines.push(
                "Hint: verify the API key, organization access, and selected model.".to_string(),
            );
        }
        return lines;
    }

    if detail.starts_with("tool denied:") {
        return vec![format!("Permission denied: {detail}")];
    }

    if lower.contains("outside writable roots")
        || lower.contains("outside readable roots")
        || lower.contains("is protected:")
    {
        return vec![format!("Path policy error: {detail}")];
    }

    if detail.starts_with("session `") && detail.ends_with("` not found") {
        return vec![format!("Session error: {detail}")];
    }

    vec![format!("Error: {detail}")]
}

fn handle_permission_command(
    command: PermissionCommands,
    config_store: &PermissionConfigStore,
) -> Result<()> {
    match command {
        PermissionCommands::Show(scope) => {
            let config = load_scope_config(config_store, scope.into_scope())?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        PermissionCommands::Export { path, scope } => {
            let config = load_scope_config(config_store, scope.into_scope())?;
            write_json_file(&path, &config)?;
            println!("{}", path.display());
        }
        PermissionCommands::Import { path, scope } => {
            let scope = scope.into_scope();
            let config = read_json_file::<PersistentPermissionConfig>(&path)?;
            save_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        PermissionCommands::SetMode { mode, scope } => {
            let scope = scope.into_scope();
            let mut config = load_scope_config(config_store, scope)?;
            config.mode = Some(mode.into());
            save_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        PermissionCommands::AllowTool { name, scope } => {
            let scope = scope.into_scope();
            let mut config = load_scope_config(config_store, scope)?;
            config.rules.allowed_tools.insert(name);
            save_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        PermissionCommands::DenyTool { name, scope } => {
            let scope = scope.into_scope();
            let mut config = load_scope_config(config_store, scope)?;
            config.rules.denied_tools.insert(name);
            save_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        PermissionCommands::AddReadableRoot { path, scope } => {
            let scope = scope.into_scope();
            let mut config = load_scope_config(config_store, scope)?;
            config
                .rules
                .readable_roots
                .push(canonicalize_existing_path(path)?);
            save_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        PermissionCommands::AddWritableRoot { path, scope } => {
            let scope = scope.into_scope();
            let mut config = load_scope_config(config_store, scope)?;
            config
                .rules
                .writable_roots
                .push(canonicalize_existing_path(path)?);
            save_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        PermissionCommands::Reset(scope) => match scope.into_scope() {
            PermissionScope::Global => {
                config_store.clear_global()?;
                println!("{{\"scope\":\"global\",\"reset\":true}}");
            }
            PermissionScope::Session(session_id) => {
                config_store.clear_session(session_id)?;
                println!(
                    "{{\"scope\":\"session\",\"session_id\":\"{session_id}\",\"reset\":true}}"
                );
            }
        },
    }

    Ok(())
}

fn handle_hook_command(command: HookCommands, config_store: &HookConfigStore) -> Result<()> {
    match command {
        HookCommands::Show(scope) => {
            let config = load_hook_scope_config(config_store, scope.into_scope())?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        HookCommands::Export { path, scope } => {
            let config = load_hook_scope_config(config_store, scope.into_scope())?;
            write_json_file(&path, &config)?;
            println!("{}", path.display());
        }
        HookCommands::Import { path, scope } => {
            let scope = scope.into_scope();
            let config = read_json_file::<PersistentHookConfig>(&path)?;
            save_hook_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        HookCommands::DenyTool {
            name,
            reason,
            scope,
        } => {
            let scope = scope.into_scope();
            let mut config = load_hook_scope_config(config_store, scope)?;
            config.denied_tools.insert(
                name.clone(),
                reason.unwrap_or_else(|| default_hook_deny_reason(&name)),
            );
            save_hook_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        HookCommands::AllowTool { name, scope } => {
            let scope = scope.into_scope();
            let mut config = load_hook_scope_config(config_store, scope)?;
            config.denied_tools.remove(&name);
            save_hook_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        HookCommands::AddTag { tag, scope } => {
            let scope = scope.into_scope();
            let mut config = load_hook_scope_config(config_store, scope)?;
            if !config.tags.contains(&tag) {
                config.tags.push(tag);
            }
            save_hook_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        HookCommands::RemoveTag { tag, scope } => {
            let scope = scope.into_scope();
            let mut config = load_hook_scope_config(config_store, scope)?;
            config.tags.retain(|existing| existing != &tag);
            save_hook_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        HookCommands::SetMetadata { key, value, scope } => {
            let scope = scope.into_scope();
            let mut config = load_hook_scope_config(config_store, scope)?;
            config.metadata.insert(key, value);
            save_hook_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        HookCommands::UnsetMetadata { key, scope } => {
            let scope = scope.into_scope();
            let mut config = load_hook_scope_config(config_store, scope)?;
            config.metadata.remove(&key);
            save_hook_scope_config(config_store, scope, &config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        HookCommands::Reset(scope) => match scope.into_scope() {
            HookScope::Global => {
                config_store.clear_global()?;
                println!("{{\"scope\":\"global\",\"reset\":true}}");
            }
            HookScope::Session(session_id) => {
                config_store.clear_session(session_id)?;
                println!(
                    "{{\"scope\":\"session\",\"session_id\":\"{session_id}\",\"reset\":true}}"
                );
            }
        },
    }

    Ok(())
}

fn load_scope_config(
    config_store: &PermissionConfigStore,
    scope: PermissionScope,
) -> Result<PersistentPermissionConfig> {
    match scope {
        PermissionScope::Global => config_store.load_global(),
        PermissionScope::Session(session_id) => config_store.load_session(session_id),
    }
}

fn load_hook_scope_config(
    config_store: &HookConfigStore,
    scope: HookScope,
) -> Result<PersistentHookConfig> {
    match scope {
        HookScope::Global => config_store.load_global(),
        HookScope::Session(session_id) => config_store.load_session(session_id),
    }
}

fn save_scope_config(
    config_store: &PermissionConfigStore,
    scope: PermissionScope,
    config: &PersistentPermissionConfig,
) -> Result<()> {
    match scope {
        PermissionScope::Global => config_store.save_global(config),
        PermissionScope::Session(session_id) => config_store.save_session(session_id, config),
    }
}

fn save_hook_scope_config(
    config_store: &HookConfigStore,
    scope: HookScope,
    config: &PersistentHookConfig,
) -> Result<()> {
    match scope {
        HookScope::Global => config_store.save_global(config),
        HookScope::Session(session_id) => config_store.save_session(session_id, config),
    }
}

fn build_tool_context(
    cli_overrides: &PersistentPermissionConfig,
    config_store: &PermissionConfigStore,
    cwd: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: Option<SessionId>,
) -> Result<ToolUseContext> {
    let mut configs = vec![load_project_permission_config(workspace_root)?];
    configs.push(config_store.load_global()?);
    if let Some(session_id) = session_id {
        configs.push(config_store.load_session(session_id)?);
    }
    configs.push(cli_overrides.clone());

    let merged = merge_permission_configs(configs);
    let mode = merged.mode.unwrap_or(PermissionMode::WorkspaceWrite);
    let policy = PermissionPolicy::new(mode, workspace_root, merged.rules)?;
    ToolUseContext::new(cwd, policy)
}

fn cli_permission_overrides(cli: &Cli) -> PersistentPermissionConfig {
    PersistentPermissionConfig {
        mode: cli.permission_mode.map(Into::into),
        rules: PermissionRules {
            readable_roots: cli.readable_root.clone(),
            writable_roots: cli.writable_root.clone(),
            allowed_tools: cli.allow_tool.iter().cloned().collect::<HashSet<_>>(),
            denied_tools: cli.deny_tool.iter().cloned().collect::<HashSet<_>>(),
        },
    }
}

fn cli_hook_overrides(cli: &Cli) -> PersistentHookConfig {
    PersistentHookConfig {
        denied_tools: cli
            .hook_deny_tool
            .iter()
            .cloned()
            .map(|tool_name| {
                let reason = default_hook_deny_reason(&tool_name);
                (tool_name, reason)
            })
            .collect(),
        tags: cli.hook_tag.clone(),
        metadata: parse_hook_metadata(&cli.hook_metadata),
    }
}

fn build_model_client(
    cli: &Cli,
    visible_tools: Vec<ToolDefinition>,
) -> Result<Arc<dyn ModelClient>> {
    match cli.model_driver {
        CliModelDriver::RuleBased => Ok(Arc::new(RuleBasedModelClient)),
        CliModelDriver::OpenaiCompatible => {
            if cli.request_timeout_seconds == 0 {
                bail!("`--request-timeout-seconds` must be greater than 0");
            }
            let api_key = env::var(&cli.api_key_env).map_err(|_| {
                anyhow::anyhow!(
                    "model driver `openai-compatible` requires env var `{}`",
                    cli.api_key_env
                )
            })?;
            let model_name = cli.model_name.clone().ok_or_else(|| {
                anyhow::anyhow!("`--model-name` is required for `openai-compatible`")
            })?;

            let mut config = OpenAiCompatibleConfig::new(api_key, model_name);
            if let Some(api_base) = &cli.api_base {
                config.api_base = api_base.clone();
            }
            config.request_timeout = Duration::from_secs(cli.request_timeout_seconds);
            config.max_retries = cli.max_model_retries;
            config.retry_backoff = Duration::from_millis(cli.retry_backoff_millis);
            config.system_prompt = cli
                .system_prompt
                .clone()
                .or_else(|| Some(default_openai_system_prompt(&visible_tools)));

            Ok(Arc::new(OpenAiCompatibleModelClient::new(config)?))
        }
    }
}

fn default_openai_system_prompt(visible_tools: &[ToolDefinition]) -> String {
    let tool_block = if visible_tools.is_empty() {
        "No tools are currently available.".to_string()
    } else {
        visible_tools
            .iter()
            .map(|tool| {
                format!(
                    "- {} [{}]: {}",
                    tool.name,
                    tool.capability.as_str(),
                    tool.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let tool_names = visible_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let workflow_block = build_default_workflow_guidance(&tool_names);
    let example_block = build_default_tool_examples(&tool_names);

    format!(
        "You are a coding agent operating in a CLI environment.\n\
Use tools when they materially help. When invoking a tool, respond with exactly one line in the form `/tool <name> <payload>` and no extra text.\n\
Use JSON payloads for structured arguments.\n\
{workflow_block}\n\
When you have enough information, reply normally with the final answer instead of calling another tool.\n\
\nAvailable tools:\n{tool_block}\n\
\nRecommended call patterns:\n{example_block}"
    )
}

fn build_default_workflow_guidance(tool_names: &std::collections::HashSet<&str>) -> String {
    let mut guidance = vec![
        "Prefer the smallest useful read before acting.".to_string(),
        "Do not rewrite whole files when a localized edit is enough.".to_string(),
    ];

    if tool_names.contains("file_read_lines") && tool_names.contains("file_edit_lines") {
        guidance.push(
            "For local code changes, first inspect with `file_read_lines`, then modify with `file_edit_lines`."
                .to_string(),
        );
    }

    if tool_names.contains("grep") || tool_names.contains("glob") {
        guidance.push(
            "Use `grep` or `glob` to find the right file before opening large files blindly."
                .to_string(),
        );
    }

    if tool_names.contains("bash") {
        guidance.push(
            "Use `bash` for verification commands only when file tools are insufficient or when you need to run project commands. Keep `bash` calls to a single non-destructive command without shell chaining or redirection."
                .to_string(),
        );
    }

    guidance.join(" ")
}

fn build_default_tool_examples(tool_names: &std::collections::HashSet<&str>) -> String {
    let mut examples = Vec::new();

    if tool_names.contains("file_read_lines") {
        examples.push(
            r#"- Read a focused range: /tool file_read_lines {"path":"src/main.rs","start_line":10,"end_line":40}"#
                .to_string(),
        );
    }
    if tool_names.contains("file_edit_lines") {
        examples.push(
            r#"- Replace a range: /tool file_edit_lines {"path":"src/main.rs","start_line":18,"end_line":22,"new_text":"replacement lines"}"#
                .to_string(),
        );
    }
    if tool_names.contains("file_edit") {
        examples.push(
            r#"- Replace exact text: /tool file_edit {"path":"src/main.rs","old_text":"before","new_text":"after"}"#
                .to_string(),
        );
    }
    if tool_names.contains("grep") {
        examples.push(r#"- Search by content: /tool grep {"pattern":"MyFunction"}"#.to_string());
    }
    if tool_names.contains("bash") {
        examples.push(r#"- Run a command: /tool bash {"command":"cargo test"}"#.to_string());
    }

    if examples.is_empty() {
        "- No tool examples available.".to_string()
    } else {
        examples.join("\n")
    }
}

#[derive(Default)]
struct StderrModelStreamSink {
    wrote_any: bool,
}

impl ModelStreamSink for StderrModelStreamSink {
    fn on_text_delta(&mut self, delta: &str) -> Result<()> {
        self.wrote_any = true;
        eprint!("{delta}");
        io::stderr().flush()?;
        Ok(())
    }

    fn on_message_end(&mut self) -> Result<()> {
        if self.wrote_any {
            eprintln!();
            io::stderr().flush()?;
            self.wrote_any = false;
        }
        Ok(())
    }
}

fn canonicalize_existing_path(path: PathBuf) -> Result<PathBuf> {
    Ok(std::fs::canonicalize(path)?)
}

fn build_tool_call(name: String, input: Option<String>, raw: Option<String>) -> Result<ToolCall> {
    if let Some(input) = input {
        return Ok(ToolCall {
            name,
            input: serde_json::from_str::<Value>(&input)?,
        });
    }

    if let Some(raw) = raw {
        let user_form = format!("/tool {name} {raw}");
        return Ok(
            tool_call_from_user_input(&user_form).unwrap_or_else(|| ToolCall {
                name,
                input: json!({ "raw": raw }),
            }),
        );
    }

    Ok(ToolCall {
        name,
        input: json!({}),
    })
}

fn build_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::default();
    tools.register(EchoTool);
    tools.register(BashTool);
    tools.register(FileReadTool);
    tools.register(FileReadLinesTool);
    tools.register(FileEditTool);
    tools.register(FileEditLinesTool);
    tools.register(FileWriteTool);
    tools.register(GlobTool);
    tools.register(GrepTool);
    tools
}

fn build_hook_bus(
    hook_log: Option<&PathBuf>,
    config_store: &HookConfigStore,
    cli_overrides: &PersistentHookConfig,
    workspace_root: &std::path::Path,
    session_id: Option<SessionId>,
) -> Result<HookBus> {
    let mut hooks = HookBus::new();
    let mut configs = vec![load_project_hook_config(workspace_root)?];
    configs.push(config_store.load_global()?);
    if let Some(session_id) = session_id {
        configs.push(config_store.load_session(session_id)?);
    }
    configs.push(cli_overrides.clone());
    let merged = merge_hook_configs(configs);

    if let Some(path) = hook_log {
        hooks.register(JsonlHook::new(path));
    }
    if !merged.denied_tools.is_empty() {
        hooks.register(DenyToolHook::new(merged.denied_tools));
    }
    if !merged.tags.is_empty() || !merged.metadata.is_empty() {
        hooks.register(SessionAnnotatingHook::new(merged.tags, merged.metadata));
    }
    Ok(hooks)
}

fn parse_hook_metadata(items: &[String]) -> BTreeMap<String, String> {
    items
        .iter()
        .filter_map(|item| item.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn default_hook_deny_reason(tool_name: &str) -> String {
    format!("blocked by hook policy for `{tool_name}`")
}

fn load_project_permission_config(
    workspace_root: &std::path::Path,
) -> Result<PersistentPermissionConfig> {
    read_optional_json_file(&project_permission_config_path(workspace_root))
}

fn load_project_hook_config(workspace_root: &std::path::Path) -> Result<PersistentHookConfig> {
    read_optional_json_file(&project_hook_config_path(workspace_root))
}

fn project_permission_config_path(workspace_root: &std::path::Path) -> PathBuf {
    workspace_root
        .join(PROJECT_CONFIG_DIRNAME)
        .join(PROJECT_PERMISSION_CONFIG_FILENAME)
}

fn project_hook_config_path(workspace_root: &std::path::Path) -> PathBuf {
    workspace_root
        .join(PROJECT_CONFIG_DIRNAME)
        .join(PROJECT_HOOK_CONFIG_FILENAME)
}

fn read_json_file<T>(path: &PathBuf) -> Result<T>
where
    T: DeserializeOwned,
{
    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
}

fn read_optional_json_file<T>(path: &PathBuf) -> Result<T>
where
    T: DeserializeOwned + Default,
{
    if !path.exists() {
        return Ok(T::default());
    }

    read_json_file(path)
}

fn write_json_file<T>(path: &PathBuf, value: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

impl From<CliPermissionMode> for PermissionMode {
    fn from(value: CliPermissionMode) -> Self {
        match value {
            CliPermissionMode::ReadOnly => Self::ReadOnly,
            CliPermissionMode::WorkspaceWrite => Self::WorkspaceWrite,
            CliPermissionMode::BypassPermissions => Self::BypassPermissions,
        }
    }
}

impl PermissionScopeArgs {
    fn into_scope(self) -> PermissionScope {
        self.session_id
            .map(PermissionScope::Session)
            .unwrap_or(PermissionScope::Global)
    }
}

impl HookScopeArgs {
    fn into_scope(self) -> HookScope {
        self.session_id
            .map(HookScope::Session)
            .unwrap_or(HookScope::Global)
    }
}

#[cfg(test)]
mod tests {
    use zetta_core::tool::{ToolCapability, ToolDefinition};

    use super::{default_openai_system_prompt, render_cli_error_lines};

    #[test]
    fn default_system_prompt_lists_visible_tools() {
        let prompt = default_openai_system_prompt(&[
            ToolDefinition {
                name: "file_read_lines".to_string(),
                description: "Reads an inclusive line range.".to_string(),
                capability: ToolCapability::Read,
            },
            ToolDefinition {
                name: "file_edit_lines".to_string(),
                description: "Replaces an inclusive line range.".to_string(),
                capability: ToolCapability::Write,
            },
        ]);

        assert!(prompt.contains("file_read_lines"));
        assert!(prompt.contains("file_edit_lines"));
        assert!(prompt.contains("respond with exactly one line"));
        assert!(prompt
            .contains("first inspect with `file_read_lines`, then modify with `file_edit_lines`"));
        assert!(prompt.contains("Read a focused range"));
    }

    #[test]
    fn default_system_prompt_handles_empty_tool_list() {
        let prompt = default_openai_system_prompt(&[]);
        assert!(prompt.contains("No tools are currently available."));
        assert!(prompt.contains("No tool examples available."));
    }

    #[test]
    fn cli_error_renderer_formats_provider_errors_with_hints() {
        let lines = render_cli_error_lines(&anyhow::anyhow!(
            "provider returned HTTP 429: rate limit exceeded"
        ));
        assert_eq!(
            lines[0],
            "Provider error: provider returned HTTP 429: rate limit exceeded"
        );
        assert!(lines[1].contains("rate limited"));
    }

    #[test]
    fn cli_error_renderer_formats_path_policy_errors() {
        let lines = render_cli_error_lines(&anyhow::anyhow!(
            "write path `/tmp/.git/config` is protected: repository metadata"
        ));
        assert_eq!(
            lines,
            vec!["Path policy error: write path `/tmp/.git/config` is protected: repository metadata"]
        );
    }
}
