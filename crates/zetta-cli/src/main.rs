mod hook_config;
mod permission_config;
mod prompting;
mod provider_config;
mod repl;
mod runtime;
mod session_view;
mod tui;

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Error, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use hook_config::{merge_hook_configs, HookConfigStore, HookScope, PersistentHookConfig};
use permission_config::{
    merge_permission_configs, PermissionConfigStore, PermissionScope, PersistentPermissionConfig,
};
use provider_config::{PersistentProviderProfile, ProviderConfigStore};
use pulldown_cmark::{
    CodeBlockKind, Event as MarkdownEvent, HeadingLevel, Options, Parser as MarkdownParser, Tag,
    TagEnd,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Terminal;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use zetta_core::engine::{AgentEngine, EngineEventSink};
use zetta_core::hook::{DenyToolHook, HookBus, JsonlHook, SessionAnnotatingHook};
use zetta_core::model::{
    summarize_tool_result, tool_call_from_user_input, ModelClient, ModelStreamSink,
    OpenAiCompatibleConfig, OpenAiCompatibleModelClient, RuleBasedModelClient,
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
const TUI_COMPOSER_HEIGHT: u16 = 8;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

use tui::run_tui;

use prompting::default_openai_system_prompt;
use repl::run_repl;
pub(crate) use runtime::{build_agent_engine, print_cli_error, run_agent_turn};
use session_view::{build_session_overview, print_session_overview, session_overview_text};
#[cfg(test)]
use tui::{
    clamp_cursor_boundary, display_width, line_end_boundary, line_start_boundary,
    next_char_boundary, pane_title, previous_char_boundary, render_markdown_styled_lines,
    split_text_lines, tui_input_history_from_session, wrap_plain_lines,
};
use tui::{
    format_elapsed, latest_assistant_message, parse_repl_command, parse_tui_slash_command,
    print_provider_summary, print_runtime_summary, print_session_history,
    print_session_search_results, print_session_summary, render_cli_error_lines,
    render_repl_prompt, search_session_messages, summarize_history_content,
    trim_session_to_last_user_turns, user_turn_from_end, StderrTurnPresenter,
};

#[derive(Parser)]
#[command(name = "zetta")]
#[command(bin_name = "zetta")]
#[command(version)]
#[command(about = "Headless Rust agent runtime for Zetta")]
#[command(
    long_about = "Zetta is a Rust agent runtime with a bounded tool loop, permission controls, session persistence, and an interactive REPL."
)]
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
    provider: Option<String>,

    #[arg(long, global = true)]
    system_prompt: Option<String>,

    #[arg(long, global = true)]
    stream_output: bool,

    #[arg(long, global = true, value_enum, default_value = "pretty")]
    ui_mode: CliUiMode,

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

impl CliPermissionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::BypassPermissions => "bypass-permissions",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::WorkspaceWrite => "rw",
            Self::BypassPermissions => "bp",
        }
    }
}

#[derive(Clone, Debug, ValueEnum)]
enum CliModelDriver {
    RuleBased,
    OpenaiCompatible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CliUiMode {
    Off,
    Pretty,
    Json,
}

impl CliUiMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Pretty => "pretty",
            Self::Json => "json",
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Run a single agent turn from a prompt")]
    Run {
        #[arg(long)]
        prompt: String,

        #[arg(long)]
        session_id: Option<SessionId>,

        #[arg(long)]
        json: bool,
    },
    #[command(about = "Start the interactive REPL")]
    Repl {
        #[arg(long)]
        session_id: Option<SessionId>,
    },
    #[command(about = "Start the full-screen terminal UI")]
    Tui {
        #[arg(long)]
        session_id: Option<SessionId>,
    },
    #[command(about = "Inspect saved sessions")]
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },
    #[command(about = "List or call tools directly")]
    Tool {
        #[command(subcommand)]
        command: ToolCommands,
    },
    #[command(about = "Manage permission policy")]
    Permission {
        #[command(subcommand)]
        command: PermissionCommands,
    },
    #[command(about = "Manage hook policy")]
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },
    #[command(about = "Manage provider profiles")]
    Provider {
        #[command(subcommand)]
        command: ProviderCommands,
    },
}

#[derive(Subcommand)]
enum SessionCommands {
    #[command(about = "Print a saved session as JSON")]
    Show {
        #[arg(long)]
        session_id: SessionId,
    },
    #[command(about = "Print a compact session overview")]
    Overview {
        #[arg(long)]
        session_id: SessionId,
    },
}

#[derive(Subcommand)]
enum ToolCommands {
    #[command(about = "List visible tools under the current permission context")]
    List {
        #[arg(long)]
        session_id: Option<SessionId>,
    },
    #[command(about = "Call a tool directly")]
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
    #[command(about = "Show the effective permission config")]
    Show(PermissionScopeArgs),
    #[command(about = "Export permission config to JSON")]
    Export {
        path: PathBuf,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    #[command(about = "Import permission config from JSON")]
    Import {
        path: PathBuf,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    #[command(about = "Set the permission mode")]
    SetMode {
        #[arg(value_enum)]
        mode: CliPermissionMode,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    #[command(about = "Allow a tool")]
    AllowTool {
        name: String,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    #[command(about = "Deny a tool")]
    DenyTool {
        name: String,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    #[command(about = "Add a readable root")]
    AddReadableRoot {
        path: PathBuf,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    #[command(about = "Add a writable root")]
    AddWritableRoot {
        path: PathBuf,
        #[command(flatten)]
        scope: PermissionScopeArgs,
    },
    #[command(about = "Reset permission config for the selected scope")]
    Reset(PermissionScopeArgs),
}

#[derive(Subcommand)]
enum HookCommands {
    #[command(about = "Show the effective hook config")]
    Show(HookScopeArgs),
    #[command(about = "Export hook config to JSON")]
    Export {
        path: PathBuf,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    #[command(about = "Import hook config from JSON")]
    Import {
        path: PathBuf,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    #[command(about = "Deny a tool through hook policy")]
    DenyTool {
        name: String,
        #[arg(long)]
        reason: Option<String>,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    #[command(about = "Remove a tool deny from hook policy")]
    AllowTool {
        name: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    #[command(about = "Add a session tag through hook policy")]
    AddTag {
        tag: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    #[command(about = "Remove a session tag from hook policy")]
    RemoveTag {
        tag: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    #[command(about = "Set a session metadata key through hook policy")]
    SetMetadata {
        key: String,
        value: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    #[command(about = "Unset a session metadata key from hook policy")]
    UnsetMetadata {
        key: String,
        #[command(flatten)]
        scope: HookScopeArgs,
    },
    #[command(about = "Reset hook config for the selected scope")]
    Reset(HookScopeArgs),
}

#[derive(Subcommand)]
enum ProviderCommands {
    #[command(about = "List saved provider profiles")]
    List,
    #[command(about = "Show one provider profile")]
    Show { name: String },
    #[command(about = "Create or update a provider profile")]
    Set {
        name: String,
        #[arg(long)]
        api_base: Option<String>,
        #[arg(long)]
        api_key_env: Option<String>,
        #[arg(long)]
        model_name: Option<String>,
        #[arg(long)]
        system_prompt: Option<String>,
    },
    #[command(about = "Remove a provider profile")]
    Remove { name: String },
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

enum ReplCommand {
    Help,
    Exit,
    Session,
    Tools,
    History,
    Search(String),
    Last,
    Write(String),
    Show,
    New,
    Reset,
    Trim(usize),
    Retry,
    Rerun(usize),
    Export(String),
    Provider,
    Config,
    Load(SessionId),
    Fork,
    ProviderUse(String),
    ProviderClear,
    ModeShow,
    ModeSet(CliPermissionMode),
    Overview,
    UiShow,
    UiSet(CliUiMode),
    EventsShow,
    EventsSet(bool),
    JsonShow,
    JsonSet(bool),
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
    let provider_config_store = ProviderConfigStore::new(&cli.config_dir);
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
            let output = run_agent_turn(
                &cli,
                store.clone(),
                &config_store,
                &hook_config_store,
                &provider_config_store,
                &cli_overrides,
                &cli_hook_overrides,
                &cwd,
                &workspace_root,
                session_id,
                cli.provider.as_deref(),
                cli.ui_mode,
                prompt,
            )
            .await?;

            for failure in &output.hook_failures {
                eprintln!("hook `{}` failed: {}", failure.handler_name, failure.error);
            }

            if json {
                for event in output.events {
                    println!("{}", serde_json::to_string(&event)?);
                }
            } else {
                println!("session_id: {}", output.session.session_id);
                if !cli.stream_output {
                    let assistant = output
                        .session
                        .messages
                        .iter()
                        .rev()
                        .find(|message| {
                            matches!(message.role, zetta_protocol::MessageRole::Assistant)
                        })
                        .map(|message| message.content.as_str())
                        .unwrap_or("<no assistant message>");
                    println!("{assistant}");
                }
            }
        }
        Commands::Repl { session_id } => {
            run_repl(
                &cli,
                store.clone(),
                &config_store,
                &hook_config_store,
                &provider_config_store,
                &cli_overrides,
                &cli_hook_overrides,
                &cwd,
                &workspace_root,
                session_id,
            )
            .await?;
        }
        Commands::Tui { session_id } => {
            run_tui(
                &cli,
                store.clone(),
                &config_store,
                &hook_config_store,
                &provider_config_store,
                &cli_overrides,
                &cli_hook_overrides,
                &cwd,
                &workspace_root,
                session_id,
            )
            .await?;
        }
        Commands::Session { command } => match command {
            SessionCommands::Show { session_id } => {
                let Some(session) = store.load(&session_id).await? else {
                    bail!("session `{session_id}` not found");
                };
                println!("{}", serde_json::to_string_pretty(&session)?);
            }
            SessionCommands::Overview { session_id } => {
                let Some(session) = store.load(&session_id).await? else {
                    bail!("session `{session_id}` not found");
                };
                print_session_overview(&session);
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
        Commands::Provider { command } => {
            handle_provider_command(command, &provider_config_store)?;
        }
    }

    Ok(())
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

fn handle_provider_command(
    command: ProviderCommands,
    config_store: &ProviderConfigStore,
) -> Result<()> {
    match command {
        ProviderCommands::List => {
            let config = config_store.load()?;
            for name in config.providers.keys() {
                println!("{name}");
            }
        }
        ProviderCommands::Show { name } => {
            let config = config_store.load()?;
            let Some(profile) = config.providers.get(&name) else {
                bail!("provider `{name}` not found");
            };
            println!("{}", serde_json::to_string_pretty(profile)?);
        }
        ProviderCommands::Set {
            name,
            api_base,
            api_key_env,
            model_name,
            system_prompt,
        } => {
            let mut config = config_store.load()?;
            config.providers.insert(
                name,
                PersistentProviderProfile {
                    api_base,
                    api_key_env,
                    model_name,
                    system_prompt,
                },
            );
            config_store.save(&config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        ProviderCommands::Remove { name } => {
            let mut config = config_store.load()?;
            config.providers.remove(&name);
            config_store.save(&config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
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

fn effective_permission_overrides(
    cli_overrides: &PersistentPermissionConfig,
    mode_override: Option<CliPermissionMode>,
) -> PersistentPermissionConfig {
    let mut overrides = cli_overrides.clone();
    if let Some(mode) = mode_override {
        overrides.mode = Some(mode.into());
    }
    overrides
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolvedOpenAiCompatibleOptions {
    api_key_env: String,
    model_name: Option<String>,
    api_base: Option<String>,
    system_prompt: Option<String>,
}

fn resolve_provider_profile_by_name(
    provider_name: Option<&str>,
    config_store: &ProviderConfigStore,
) -> Result<Option<PersistentProviderProfile>> {
    let Some(provider_name) = provider_name else {
        return Ok(None);
    };

    let config = config_store.load()?;
    let Some(profile) = config.providers.get(provider_name) else {
        bail!("provider `{provider_name}` not found");
    };

    Ok(Some(profile.clone()))
}

fn resolve_openai_options(
    cli: &Cli,
    provider_profile: Option<&PersistentProviderProfile>,
) -> ResolvedOpenAiCompatibleOptions {
    let api_key_env = if cli.api_key_env != "OPENAI_API_KEY" {
        cli.api_key_env.clone()
    } else {
        provider_profile
            .and_then(|profile| profile.api_key_env.clone())
            .unwrap_or_else(|| cli.api_key_env.clone())
    };

    ResolvedOpenAiCompatibleOptions {
        api_key_env,
        model_name: cli
            .model_name
            .clone()
            .or_else(|| provider_profile.and_then(|profile| profile.model_name.clone())),
        api_base: cli
            .api_base
            .clone()
            .or_else(|| provider_profile.and_then(|profile| profile.api_base.clone())),
        system_prompt: cli
            .system_prompt
            .clone()
            .or_else(|| provider_profile.and_then(|profile| profile.system_prompt.clone())),
    }
}

fn build_model_client(
    cli: &Cli,
    visible_tools: Vec<ToolDefinition>,
    provider_profile: Option<&PersistentProviderProfile>,
) -> Result<Arc<dyn ModelClient>> {
    let use_openai_compatible =
        matches!(cli.model_driver, CliModelDriver::OpenaiCompatible) || provider_profile.is_some();

    if !use_openai_compatible {
        return Ok(Arc::new(RuleBasedModelClient));
    }

    if cli.request_timeout_seconds == 0 {
        bail!("`--request-timeout-seconds` must be greater than 0");
    }

    let resolved = resolve_openai_options(cli, provider_profile);
    let api_key = env::var(&resolved.api_key_env).map_err(|_| {
        anyhow::anyhow!(
            "model driver `openai-compatible` requires env var `{}`",
            resolved.api_key_env
        )
    })?;
    let model_name = resolved
        .model_name
        .ok_or_else(|| anyhow::anyhow!("`--model-name` is required for `openai-compatible`"))?;

    let default_system_prompt = default_openai_system_prompt(&visible_tools);
    let mut config = OpenAiCompatibleConfig::new(api_key, model_name);
    if let Some(api_base) = resolved.api_base {
        config.api_base = api_base;
    }
    config.tools = visible_tools;
    config.request_timeout = Duration::from_secs(cli.request_timeout_seconds);
    config.max_retries = cli.max_model_retries;
    config.retry_backoff = Duration::from_millis(cli.retry_backoff_millis);
    config.system_prompt = resolved.system_prompt.or(Some(default_system_prompt));

    Ok(Arc::new(OpenAiCompatibleModelClient::new(config)?))
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

fn write_text_file(path: &PathBuf, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
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
    use clap::Parser;
    use zetta_core::tool::{ToolCapability, ToolDefinition};
    use zetta_protocol::SessionId;

    use crate::provider_config::PersistentProviderProfile;
    use crate::{CliPermissionMode, CliUiMode};
    use ratatui::style::{Color, Style};

    use super::{
        build_session_overview, clamp_cursor_boundary, default_openai_system_prompt, display_width,
        format_elapsed, latest_assistant_message, line_end_boundary, line_start_boundary,
        next_char_boundary, pane_title, parse_repl_command, parse_tui_slash_command,
        previous_char_boundary, render_cli_error_lines, render_markdown_styled_lines,
        render_repl_prompt, resolve_openai_options, search_session_messages, split_text_lines,
        summarize_history_content, trim_session_to_last_user_turns, tui_input_history_from_session,
        user_turn_from_end, wrap_plain_lines, ReplCommand, ResolvedOpenAiCompatibleOptions,
    };

    #[test]
    fn default_system_prompt_lists_visible_tools() {
        let prompt = default_openai_system_prompt(&[
            ToolDefinition {
                name: "glob".to_string(),
                description: "Matches files by wildcard.".to_string(),
                capability: ToolCapability::Read,
            },
            ToolDefinition {
                name: "grep".to_string(),
                description: "Searches file contents.".to_string(),
                capability: ToolCapability::Read,
            },
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
            ToolDefinition {
                name: "bash".to_string(),
                description: "Runs one shell command.".to_string(),
                capability: ToolCapability::Execute,
            },
        ]);

        assert!(prompt.contains("file_read_lines"));
        assert!(prompt.contains("file_edit_lines"));
        assert!(prompt.contains("respond with exactly one line"));
        assert!(prompt
            .contains("first inspect with `file_read_lines`, then modify with `file_edit_lines`"));
        assert!(prompt.contains("prefer `glob` instead of `bash`"));
        assert!(prompt.contains("prefer `grep` instead of shell `find`/`grep` pipelines"));
        assert!(prompt.contains("Do not use shell pipelines"));
        assert!(prompt.contains("List Rust files"));
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

    #[test]
    fn tui_line_helpers_measure_wide_characters_by_display_width() {
        assert_eq!(display_width("仓库"), 4);
        assert_eq!(
            wrap_plain_lines(&[String::from("仓库结构")], 4),
            vec!["仓库", "结构"]
        );
        assert_eq!(
            split_text_lines("line one\nline two\n"),
            vec!["line one", "line two", ""]
        );
    }

    #[test]
    fn pane_title_surfaces_paused_and_new_counts() {
        assert_eq!(pane_title("Conversation", 0, 0), " Conversation ");
        assert_eq!(pane_title("Conversation", 5, 0), " Conversation • paused ");
        assert_eq!(pane_title("Activity", 3, 2), " Activity • paused • 2 new ");
    }

    #[test]
    fn markdown_renderer_formats_headings_lists_and_code_blocks() {
        let lines = render_markdown_styled_lines(
            "# Title\n\n- one\n- two\n\n```rust\nfn main() {}\n```",
            Style::default().fg(Color::White),
        );
        let rendered = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert!(rendered.contains(&"  Title"));
        assert!(rendered.contains(&"  • one"));
        assert!(rendered.contains(&"  • two"));
        assert!(rendered.contains(&"  ```rust"));
        assert!(rendered.contains(&"    fn main() {}"));
    }

    #[test]
    fn elapsed_formatter_uses_compact_human_readable_units() {
        assert_eq!(format_elapsed(9), "9s");
        assert_eq!(format_elapsed(65), "1m 05s");
        assert_eq!(format_elapsed(3665), "1h 01m 05s");
    }

    #[test]
    fn cursor_helpers_follow_utf8_and_line_boundaries() {
        let input = "ab\n仓库 cd";
        let cursor = clamp_cursor_boundary(input, input.len());
        assert_eq!(line_start_boundary(input, cursor), "ab\n".len());
        assert_eq!(line_end_boundary(input, 0), 2);
        assert_eq!(
            previous_char_boundary(input, input.len()),
            Some(input.len() - 1)
        );
        let ideograph_start = "ab\n".len();
        assert_eq!(
            next_char_boundary(input, ideograph_start),
            Some(ideograph_start + "仓".len())
        );
    }

    #[test]
    fn repl_command_parser_recognizes_local_commands() {
        assert!(matches!(
            parse_repl_command(":help"),
            Some(Ok(ReplCommand::Help))
        ));
        assert!(matches!(
            parse_repl_command(":exit"),
            Some(Ok(ReplCommand::Exit))
        ));
        assert!(matches!(
            parse_repl_command(":quit"),
            Some(Ok(ReplCommand::Exit))
        ));
        assert!(matches!(
            parse_repl_command(":session"),
            Some(Ok(ReplCommand::Session))
        ));
        assert!(matches!(
            parse_repl_command(":tools"),
            Some(Ok(ReplCommand::Tools))
        ));
        assert!(matches!(
            parse_repl_command(":history"),
            Some(Ok(ReplCommand::History))
        ));
        assert!(matches!(
            parse_repl_command(":search auth"),
            Some(Ok(ReplCommand::Search(query))) if query == "auth"
        ));
        assert!(matches!(
            parse_repl_command(":last"),
            Some(Ok(ReplCommand::Last))
        ));
        assert!(matches!(
            parse_repl_command(":write /tmp/answer.txt"),
            Some(Ok(ReplCommand::Write(path))) if path == "/tmp/answer.txt"
        ));
        assert!(matches!(
            parse_repl_command(":show"),
            Some(Ok(ReplCommand::Show))
        ));
        assert!(matches!(
            parse_repl_command(":new"),
            Some(Ok(ReplCommand::New))
        ));
        assert!(matches!(
            parse_repl_command(":reset"),
            Some(Ok(ReplCommand::Reset))
        ));
        assert!(matches!(
            parse_repl_command(":trim 2"),
            Some(Ok(ReplCommand::Trim(2)))
        ));
        assert!(matches!(
            parse_repl_command(":retry"),
            Some(Ok(ReplCommand::Retry))
        ));
        assert!(matches!(
            parse_repl_command(":rerun 2"),
            Some(Ok(ReplCommand::Rerun(2)))
        ));
        assert!(matches!(
            parse_repl_command(":export /tmp/session.json"),
            Some(Ok(ReplCommand::Export(path))) if path == "/tmp/session.json"
        ));
        assert!(matches!(
            parse_repl_command(":provider"),
            Some(Ok(ReplCommand::Provider))
        ));
        assert!(matches!(
            parse_repl_command(":provider clear"),
            Some(Ok(ReplCommand::ProviderClear))
        ));
        assert!(matches!(
            parse_repl_command(":provider use deepseek"),
            Some(Ok(ReplCommand::ProviderUse(name))) if name == "deepseek"
        ));
        assert!(matches!(
            parse_repl_command(":config"),
            Some(Ok(ReplCommand::Config))
        ));
        assert!(matches!(
            parse_repl_command(":overview"),
            Some(Ok(ReplCommand::Overview))
        ));
        assert!(matches!(
            parse_repl_command(":fork"),
            Some(Ok(ReplCommand::Fork))
        ));
        assert!(matches!(
            parse_repl_command(":ui"),
            Some(Ok(ReplCommand::UiShow))
        ));
        assert!(matches!(
            parse_repl_command(":ui pretty"),
            Some(Ok(ReplCommand::UiSet(CliUiMode::Pretty)))
        ));
        assert!(matches!(
            parse_repl_command(":mode"),
            Some(Ok(ReplCommand::ModeShow))
        ));
        assert!(matches!(
            parse_repl_command(":mode read-only"),
            Some(Ok(ReplCommand::ModeSet(CliPermissionMode::ReadOnly)))
        ));
        assert!(matches!(
            parse_repl_command(":events"),
            Some(Ok(ReplCommand::EventsShow))
        ));
        assert!(matches!(
            parse_repl_command(":events on"),
            Some(Ok(ReplCommand::EventsSet(true)))
        ));
        assert!(matches!(
            parse_repl_command(":json"),
            Some(Ok(ReplCommand::JsonShow))
        ));
        assert!(matches!(
            parse_repl_command(":json off"),
            Some(Ok(ReplCommand::JsonSet(false)))
        ));
        assert!(parse_repl_command("explain this file").is_none());
    }

    #[test]
    fn repl_command_parser_accepts_load_with_session_id() {
        let parsed =
            parse_repl_command(":load 11111111-1111-1111-1111-111111111111").expect("command");
        assert!(matches!(parsed, Ok(ReplCommand::Load(_))));
    }

    #[test]
    fn tui_slash_command_parser_recognizes_local_commands() {
        assert!(matches!(
            parse_tui_slash_command("/help"),
            Some(Ok(ReplCommand::Help))
        ));
        assert!(matches!(
            parse_tui_slash_command("/new"),
            Some(Ok(ReplCommand::New))
        ));
        assert!(matches!(
            parse_tui_slash_command("/provider use deepseek"),
            Some(Ok(ReplCommand::ProviderUse(name))) if name == "deepseek"
        ));
        assert!(matches!(
            parse_tui_slash_command("/mode workspace-write"),
            Some(Ok(ReplCommand::ModeSet(CliPermissionMode::WorkspaceWrite)))
        ));
        assert!(parse_tui_slash_command("hello").is_none());
    }

    #[test]
    fn repl_command_parser_rejects_invalid_load_inputs() {
        let missing = parse_repl_command(":load").expect("command");
        let invalid = parse_repl_command(":load not-a-uuid").expect("command");
        let export_missing = parse_repl_command(":export").expect("command");
        let search_missing = parse_repl_command(":search").expect("command");
        let write_missing = parse_repl_command(":write").expect("command");
        let trim_missing = parse_repl_command(":trim").expect("command");
        let trim_zero = parse_repl_command(":trim 0").expect("command");
        let rerun_missing = parse_repl_command(":rerun").expect("command");
        let rerun_zero = parse_repl_command(":rerun 0").expect("command");

        assert!(matches!(missing, Err(error) if error.contains(":load <session_id>")));
        assert!(matches!(invalid, Err(error) if error.contains("invalid session id")));
        assert!(matches!(export_missing, Err(error) if error.contains(":export <path>")));
        assert!(matches!(search_missing, Err(error) if error.contains(":search <text>")));
        assert!(matches!(write_missing, Err(error) if error.contains(":write <path>")));
        assert!(matches!(trim_missing, Err(error) if error.contains(":trim <turns>")));
        assert!(matches!(trim_zero, Err(error) if error.contains("positive integer")));
        assert!(matches!(rerun_missing, Err(error) if error.contains(":rerun <turns_back>")));
        assert!(matches!(rerun_zero, Err(error) if error.contains("positive integer")));
    }

    #[test]
    fn repl_command_parser_rejects_invalid_provider_and_mode_inputs() {
        let provider_missing = parse_repl_command(":provider use").expect("command");
        let provider_unknown = parse_repl_command(":provider nope").expect("command");
        let mode_invalid = parse_repl_command(":mode invalid-mode").expect("command");
        let ui_invalid = parse_repl_command(":ui invalid").expect("command");
        let events_invalid = parse_repl_command(":events maybe").expect("command");
        let json_invalid = parse_repl_command(":json maybe").expect("command");

        assert!(matches!(provider_missing, Err(error) if error.contains(":provider use <name>")));
        assert!(
            matches!(provider_unknown, Err(error) if error.contains("unknown provider subcommand"))
        );
        assert!(matches!(mode_invalid, Err(error) if error.contains("invalid mode")));
        assert!(matches!(ui_invalid, Err(error) if error.contains("invalid ui mode")));
        assert!(matches!(events_invalid, Err(error) if error.contains("invalid toggle")));
        assert!(matches!(json_invalid, Err(error) if error.contains("invalid toggle")));
    }

    #[test]
    fn user_turn_from_end_returns_requested_user_message() {
        let mut session = zetta_protocol::SessionSnapshot::new(SessionId::new());
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            "first",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Assistant,
            "reply",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            "second",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Tool,
            "tool output",
        ));

        let latest = user_turn_from_end(&session, 1);
        let previous = user_turn_from_end(&session, 2);
        let missing = user_turn_from_end(&session, 3);

        assert!(matches!(latest, Some((2, prompt)) if prompt == "second"));
        assert!(matches!(previous, Some((0, prompt)) if prompt == "first"));
        assert!(missing.is_none());
    }

    #[test]
    fn summarize_history_content_collapses_whitespace_and_truncates() {
        let summary = summarize_history_content(
            "first line\n\nsecond   line with    extra spaces and a very long tail that should be truncated cleanly with additional words to force the summary past one hundred characters",
        );

        assert!(summary.starts_with("first line second line with extra spaces"));
        assert!(summary.ends_with("..."));
        assert!(summary.len() <= 103);
    }

    #[test]
    fn latest_assistant_message_returns_last_assistant_content() {
        let mut session = zetta_protocol::SessionSnapshot::new(SessionId::new());
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            "first",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Assistant,
            "reply one",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Tool,
            "tool output",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Assistant,
            "reply two",
        ));

        assert_eq!(latest_assistant_message(&session), Some("reply two"));
    }

    #[test]
    fn search_session_messages_matches_case_insensitively() {
        let mut session = zetta_protocol::SessionSnapshot::new(SessionId::new());
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            "Find Auth flow",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Assistant,
            "Authentication succeeded",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Tool,
            "grep output",
        ));

        let matches = search_session_messages(&session, "auth");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].0, 0);
        assert_eq!(matches[0].1, "user");
        assert_eq!(matches[1].0, 1);
        assert_eq!(matches[1].1, "assistant");
    }

    #[test]
    fn trim_session_to_last_user_turns_keeps_recent_turn_boundary() {
        let mut session = zetta_protocol::SessionSnapshot::new(SessionId::new());
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            "first",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Assistant,
            "reply one",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            "second",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Tool,
            "tool output",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Assistant,
            "reply two",
        ));

        let removed = trim_session_to_last_user_turns(&mut session, 1);
        assert_eq!(removed, 2);
        assert_eq!(session.messages.len(), 3);
        assert!(matches!(
            session.messages.first().map(|message| message.role),
            Some(zetta_protocol::MessageRole::User)
        ));
        assert_eq!(session.messages[0].content, "second");
    }

    #[test]
    fn render_repl_prompt_shows_short_session_mode_and_provider() {
        let prompt = render_repl_prompt(
            "11111111-1111-1111-1111-111111111111"
                .parse()
                .expect("session id"),
            Some("deepseek"),
            Some(CliPermissionMode::ReadOnly),
        );

        assert_eq!(prompt, "zetta[11111111 ro deepseek]> ");
    }

    #[test]
    fn tui_history_seed_uses_non_empty_user_messages() {
        let mut session = zetta_protocol::SessionSnapshot::new(SessionId::new());
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::System,
            "system",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            " first prompt ",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Assistant,
            "reply",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            "",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            "second prompt",
        ));

        let history = tui_input_history_from_session(Some(&session));
        assert_eq!(history, vec!["first prompt", "second prompt"]);
    }

    #[test]
    fn session_overview_counts_tool_statuses() {
        let mut session = zetta_protocol::SessionSnapshot::new(SessionId::new());
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::User,
            "inspect auth",
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Tool,
            serde_json::to_string_pretty(&serde_json::json!({
                "type": "tool_result",
                "tool_name": "grep",
                "status": "completed",
                "output": { "matches": 2 }
            }))
            .expect("tool result"),
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Tool,
            serde_json::to_string_pretty(&serde_json::json!({
                "type": "tool_result",
                "tool_name": "bash",
                "status": "failed",
                "error": "spawn error"
            }))
            .expect("tool result"),
        ));
        session.messages.push(zetta_protocol::Message::new(
            zetta_protocol::MessageRole::Assistant,
            "done",
        ));

        let overview = build_session_overview(&session);
        assert_eq!(overview.user_turns, 1);
        assert_eq!(overview.assistant_messages, 1);
        assert_eq!(overview.tool_messages, 2);
        assert_eq!(overview.completed_tools, 1);
        assert_eq!(overview.failed_tools, 1);
        assert_eq!(overview.tool_usage.get("grep"), Some(&1));
        assert_eq!(overview.tool_usage.get("bash"), Some(&1));
    }

    #[test]
    fn provider_profile_supplies_openai_defaults() {
        let cli = super::Cli::parse_from([
            "zetta",
            "--provider",
            "deepseek",
            "run",
            "--prompt",
            "hello",
        ]);
        let resolved = resolve_openai_options(
            &cli,
            Some(&PersistentProviderProfile {
                api_base: Some("https://api.deepseek.com".to_string()),
                api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
                model_name: Some("deepseek-chat".to_string()),
                system_prompt: Some("provider prompt".to_string()),
            }),
        );

        assert_eq!(
            resolved,
            ResolvedOpenAiCompatibleOptions {
                api_key_env: "DEEPSEEK_API_KEY".to_string(),
                model_name: Some("deepseek-chat".to_string()),
                api_base: Some("https://api.deepseek.com".to_string()),
                system_prompt: Some("provider prompt".to_string()),
            }
        );
    }

    #[test]
    fn cli_flags_override_provider_defaults() {
        let cli = super::Cli::parse_from([
            "zetta",
            "--provider",
            "deepseek",
            "--api-key-env",
            "CUSTOM_KEY",
            "--api-base",
            "https://override.example.com/v1",
            "--model-name",
            "override-model",
            "--system-prompt",
            "cli prompt",
            "run",
            "--prompt",
            "hello",
        ]);
        let resolved = resolve_openai_options(
            &cli,
            Some(&PersistentProviderProfile {
                api_base: Some("https://api.deepseek.com".to_string()),
                api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
                model_name: Some("deepseek-chat".to_string()),
                system_prompt: Some("provider prompt".to_string()),
            }),
        );

        assert_eq!(resolved.api_key_env, "CUSTOM_KEY");
        assert_eq!(resolved.model_name.as_deref(), Some("override-model"));
        assert_eq!(
            resolved.api_base.as_deref(),
            Some("https://override.example.com/v1")
        );
        assert_eq!(resolved.system_prompt.as_deref(), Some("cli prompt"));
    }
}
