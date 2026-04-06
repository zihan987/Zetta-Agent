mod hook_config;
mod permission_config;
mod provider_config;

use std::collections::BTreeMap;
use std::collections::HashSet;
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

fn print_cli_error(error: &Error) {
    for line in render_cli_error_lines(error) {
        eprintln!("{line}");
    }
}

fn build_agent_engine(
    cli: &Cli,
    store: Arc<FileSessionStore>,
    config_store: &PermissionConfigStore,
    hook_config_store: &HookConfigStore,
    provider_config_store: &ProviderConfigStore,
    cli_overrides: &PersistentPermissionConfig,
    cli_hook_overrides: &PersistentHookConfig,
    cwd: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: Option<SessionId>,
    active_provider: Option<&str>,
) -> Result<AgentEngine> {
    let hook_bus = build_hook_bus(
        cli.hook_log.as_ref(),
        hook_config_store,
        cli_hook_overrides,
        workspace_root,
        session_id,
    )?;
    let tool_context =
        build_tool_context(cli_overrides, config_store, cwd, workspace_root, session_id)?;
    let registry = build_registry();
    let provider_profile =
        resolve_provider_profile_by_name(active_provider, provider_config_store)?;
    let model = build_model_client(
        cli,
        registry.visible_definitions(&tool_context),
        provider_profile.as_ref(),
    )?;
    Ok(AgentEngine::new(
        model,
        store,
        registry,
        tool_context,
        hook_bus,
    ))
}

async fn run_agent_turn(
    cli: &Cli,
    store: Arc<FileSessionStore>,
    config_store: &PermissionConfigStore,
    hook_config_store: &HookConfigStore,
    provider_config_store: &ProviderConfigStore,
    cli_overrides: &PersistentPermissionConfig,
    cli_hook_overrides: &PersistentHookConfig,
    cwd: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: Option<SessionId>,
    active_provider: Option<&str>,
    ui_mode: CliUiMode,
    prompt: &str,
) -> Result<zetta_core::engine::RunTurnOutput> {
    let engine = build_agent_engine(
        cli,
        store,
        config_store,
        hook_config_store,
        provider_config_store,
        cli_overrides,
        cli_hook_overrides,
        cwd,
        workspace_root,
        session_id,
        active_provider,
    )?;
    let request = TurnRequest {
        session_id,
        prompt: prompt.to_string(),
    };
    let mut presenter =
        StderrTurnPresenter::new(ui_mode, active_provider, cli.stream_output, prompt);
    if cli.stream_output {
        let mut sink = StderrModelStreamSink::default();
        engine
            .run_turn_with_sinks(request, Some(&mut sink), Some(&mut presenter))
            .await
    } else {
        engine
            .run_turn_with_sinks(request, None, Some(&mut presenter))
            .await
    }
}

async fn run_repl(
    cli: &Cli,
    store: Arc<FileSessionStore>,
    config_store: &PermissionConfigStore,
    hook_config_store: &HookConfigStore,
    provider_config_store: &ProviderConfigStore,
    cli_overrides: &PersistentPermissionConfig,
    cli_hook_overrides: &PersistentHookConfig,
    cwd: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: Option<SessionId>,
) -> Result<()> {
    let mut session_id = session_id.unwrap_or_default();
    let mut current_provider = cli.provider.clone();
    let mut current_permission_mode = cli.permission_mode;
    let mut current_ui_mode = cli.ui_mode;
    let mut line = String::new();

    println!("Zetta REPL");
    println!("session_id: {session_id}");
    println!("Type `:help` for local commands. Submit an empty line to skip.");

    loop {
        print!(
            "{}",
            render_repl_prompt(
                session_id,
                current_provider.as_deref(),
                current_permission_mode
            )
        );
        io::stdout().flush()?;
        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            println!();
            break;
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        if let Some(command) = parse_repl_command(input) {
            let command = match command {
                Ok(command) => command,
                Err(error) => {
                    eprintln!("REPL command error: {error}");
                    continue;
                }
            };
            match command {
                ReplCommand::Help => {
                    println!("Local commands:");
                    println!("  :help     Show this help");
                    println!("  :exit     Exit the REPL");
                    println!("  :quit     Exit the REPL");
                    println!("  :session  Print the current session id");
                    println!("  :tools    List visible tools");
                    println!("  :history  Show a compact session history");
                    println!("  :search <text>  Search the current session history");
                    println!("  :last     Show the latest assistant reply");
                    println!("  :write <path>  Write the latest assistant reply to a file");
                    println!("  :show     Print the current session transcript");
                    println!("  :new      Switch to a fresh session id");
                    println!("  :reset    Clear the current session history");
                    println!("  :trim <turns>  Keep only the most recent user turns");
                    println!("  :retry    Re-run the latest user turn");
                    println!("  :rerun <turns_back>  Re-run an earlier user turn");
                    println!("  :export <path>  Export the current session as JSON");
                    println!("  :provider Show the current provider profile");
                    println!("  :provider use <name>  Switch to a provider profile");
                    println!("  :provider clear       Clear the active provider profile");
                    println!("  :config   Show the current runtime summary");
                    println!("  :mode     Show the current permission mode");
                    println!("  :mode <read-only|workspace-write|bypass-permissions>");
                    println!("  :overview Show a compact session overview");
                    println!("  :ui       Show the current terminal UI mode");
                    println!("  :ui <off|pretty|json>");
                    println!("  :events   Show pretty event tracing status");
                    println!("  :events on|off");
                    println!("  :json     Show JSON event output status");
                    println!("  :json on|off");
                    println!("  :load <session_id>  Switch to an existing session");
                    println!("  :fork     Copy the current session into a new session id");
                }
                ReplCommand::Exit => break,
                ReplCommand::Session => println!("session_id: {session_id}"),
                ReplCommand::Tools => {
                    let tool_context = build_tool_context(
                        cli_overrides,
                        config_store,
                        cwd,
                        workspace_root,
                        Some(session_id),
                    )?;
                    for name in build_registry().visible_names(&tool_context) {
                        println!("{name}");
                    }
                }
                ReplCommand::History => match store.load(&session_id).await? {
                    Some(session) => print_session_history(&session),
                    None => println!("session_id: {session_id}\n<empty session>"),
                },
                ReplCommand::Search(query) => match store.load(&session_id).await? {
                    Some(session) => print_session_search_results(&session, &query),
                    None => println!("session_id: {session_id}\n<empty session>"),
                },
                ReplCommand::Last => match store.load(&session_id).await? {
                    Some(session) => match latest_assistant_message(&session) {
                        Some(content) => println!("{content}"),
                        None => println!("<no assistant message>"),
                    },
                    None => println!("session_id: {session_id}\n<empty session>"),
                },
                ReplCommand::Write(path) => {
                    let Some(session) = store.load(&session_id).await? else {
                        eprintln!("Session error: current session is empty");
                        continue;
                    };
                    let Some(content) = latest_assistant_message(&session) else {
                        eprintln!("Session error: no assistant message to write");
                        continue;
                    };
                    let path = PathBuf::from(path);
                    write_text_file(&path, content)?;
                    println!("wrote latest assistant reply to {}", path.display());
                }
                ReplCommand::Show => match store.load(&session_id).await? {
                    Some(session) => print_session_summary(&session),
                    None => println!("session_id: {session_id}\n<empty session>"),
                },
                ReplCommand::New => {
                    session_id = SessionId::new();
                    println!("session_id: {session_id}");
                }
                ReplCommand::Reset => {
                    store.delete(&session_id).await?;
                    println!("session_id: {session_id}");
                    println!("Current session history cleared.");
                }
                ReplCommand::Trim(turns) => {
                    let Some(mut session) = store.load(&session_id).await? else {
                        eprintln!("Session error: current session is empty");
                        continue;
                    };
                    let original_len = session.messages.len();
                    let trimmed = trim_session_to_last_user_turns(&mut session, turns);
                    if trimmed == 0 {
                        println!("No messages trimmed.");
                        continue;
                    }
                    session.updated_at = Utc::now();
                    store.save(&session).await?;
                    println!(
                        "trimmed {trimmed} messages; kept {} messages across the last {turns} user turns",
                        original_len - trimmed
                    );
                }
                ReplCommand::Retry => {
                    let Some(mut session) = store.load(&session_id).await? else {
                        eprintln!("Session error: current session is empty");
                        continue;
                    };
                    let Some((retry_index, retry_prompt)) = user_turn_from_end(&session, 1) else {
                        eprintln!("Session error: no prior user turn to retry");
                        continue;
                    };
                    session.messages.truncate(retry_index);
                    session.updated_at = Utc::now();
                    store.save(&session).await?;
                    println!("retrying latest user turn...");

                    let output = match run_agent_turn(
                        cli,
                        store.clone(),
                        config_store,
                        hook_config_store,
                        provider_config_store,
                        &effective_permission_overrides(cli_overrides, current_permission_mode),
                        cli_hook_overrides,
                        cwd,
                        workspace_root,
                        Some(session_id),
                        current_provider.as_deref(),
                        current_ui_mode,
                        &retry_prompt,
                    )
                    .await
                    {
                        Ok(output) => output,
                        Err(error) => {
                            print_cli_error(&error);
                            continue;
                        }
                    };

                    for failure in &output.hook_failures {
                        eprintln!("hook `{}` failed: {}", failure.handler_name, failure.error);
                    }

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
                ReplCommand::Rerun(turns_back) => {
                    let Some(mut session) = store.load(&session_id).await? else {
                        eprintln!("Session error: current session is empty");
                        continue;
                    };
                    let Some((rerun_index, rerun_prompt)) =
                        user_turn_from_end(&session, turns_back)
                    else {
                        eprintln!(
                            "Session error: user turn `{turns_back}` from the end was not found"
                        );
                        continue;
                    };
                    session.messages.truncate(rerun_index);
                    session.updated_at = Utc::now();
                    store.save(&session).await?;
                    println!("rerunning user turn {turns_back} from the end...");

                    let output = match run_agent_turn(
                        cli,
                        store.clone(),
                        config_store,
                        hook_config_store,
                        provider_config_store,
                        &effective_permission_overrides(cli_overrides, current_permission_mode),
                        cli_hook_overrides,
                        cwd,
                        workspace_root,
                        Some(session_id),
                        current_provider.as_deref(),
                        current_ui_mode,
                        &rerun_prompt,
                    )
                    .await
                    {
                        Ok(output) => output,
                        Err(error) => {
                            print_cli_error(&error);
                            continue;
                        }
                    };

                    for failure in &output.hook_failures {
                        eprintln!("hook `{}` failed: {}", failure.handler_name, failure.error);
                    }

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
                ReplCommand::Export(path) => {
                    let Some(session) = store.load(&session_id).await? else {
                        eprintln!("Session error: current session is empty");
                        continue;
                    };
                    let path = PathBuf::from(path);
                    write_json_file(&path, &session)?;
                    println!("exported session to {}", path.display());
                }
                ReplCommand::Provider => {
                    print_provider_summary(current_provider.as_deref(), provider_config_store)?;
                }
                ReplCommand::Config => {
                    print_runtime_summary(
                        cli,
                        config_store,
                        cli_overrides,
                        cwd,
                        workspace_root,
                        session_id,
                        current_provider.as_deref(),
                        current_permission_mode,
                        current_ui_mode,
                    )?;
                }
                ReplCommand::Overview => match store.load(&session_id).await? {
                    Some(session) => print_session_overview(&session),
                    None => println!("session_id: {session_id}\n<empty session>"),
                },
                ReplCommand::Load(target_session_id) => match store.load(&target_session_id).await?
                {
                    Some(_) => {
                        session_id = target_session_id;
                        println!("session_id: {session_id}");
                    }
                    None => {
                        eprintln!("Session error: session `{target_session_id}` not found");
                    }
                },
                ReplCommand::Fork => match store.load(&session_id).await? {
                    Some(mut session) => {
                        let source_session_id = session_id;
                        let forked_session_id = SessionId::new();
                        let now = Utc::now();
                        session.session_id = forked_session_id;
                        session.created_at = now;
                        session.updated_at = now;
                        store.save(&session).await?;
                        session_id = forked_session_id;
                        println!("forked session: {source_session_id} -> {session_id}");
                    }
                    None => {
                        let source_session_id = session_id;
                        session_id = SessionId::new();
                        println!("forked empty session: {source_session_id} -> {session_id}");
                    }
                },
                ReplCommand::ProviderUse(provider_name) => {
                    if resolve_provider_profile_by_name(
                        Some(&provider_name),
                        provider_config_store,
                    )?
                    .is_some()
                    {
                        current_provider = Some(provider_name);
                        println!(
                            "provider: {}",
                            current_provider.as_deref().unwrap_or("<none>")
                        );
                    }
                }
                ReplCommand::ProviderClear => {
                    current_provider = None;
                    println!("provider: <none>");
                }
                ReplCommand::ModeShow => {
                    let mode = current_permission_mode.unwrap_or(CliPermissionMode::WorkspaceWrite);
                    println!("permission_mode: {}", mode.as_str());
                }
                ReplCommand::ModeSet(mode) => {
                    current_permission_mode = Some(mode);
                    println!("permission_mode: {}", mode.as_str());
                }
                ReplCommand::UiShow => {
                    println!("ui: {}", current_ui_mode.as_str());
                }
                ReplCommand::UiSet(mode) => {
                    current_ui_mode = mode;
                    println!("ui: {}", current_ui_mode.as_str());
                }
                ReplCommand::EventsShow => {
                    println!(
                        "events: {}",
                        if current_ui_mode == CliUiMode::Pretty {
                            "on"
                        } else {
                            "off"
                        }
                    );
                }
                ReplCommand::EventsSet(enabled) => {
                    current_ui_mode = if enabled {
                        CliUiMode::Pretty
                    } else {
                        CliUiMode::Off
                    };
                    println!("events: {}", if enabled { "on" } else { "off" });
                }
                ReplCommand::JsonShow => {
                    println!(
                        "json: {}",
                        if current_ui_mode == CliUiMode::Json {
                            "on"
                        } else {
                            "off"
                        }
                    );
                }
                ReplCommand::JsonSet(enabled) => {
                    current_ui_mode = if enabled {
                        CliUiMode::Json
                    } else {
                        CliUiMode::Off
                    };
                    println!("json: {}", if enabled { "on" } else { "off" });
                }
            }
            continue;
        }

        let output = match run_agent_turn(
            cli,
            store.clone(),
            config_store,
            hook_config_store,
            provider_config_store,
            &effective_permission_overrides(cli_overrides, current_permission_mode),
            cli_hook_overrides,
            cwd,
            workspace_root,
            Some(session_id),
            current_provider.as_deref(),
            current_ui_mode,
            input,
        )
        .await
        {
            Ok(output) => output,
            Err(error) => {
                print_cli_error(&error);
                continue;
            }
        };

        for failure in &output.hook_failures {
            eprintln!("hook `{}` failed: {}", failure.handler_name, failure.error);
        }

        if !cli.stream_output {
            let assistant = output
                .session
                .messages
                .iter()
                .rev()
                .find(|message| matches!(message.role, zetta_protocol::MessageRole::Assistant))
                .map(|message| message.content.as_str())
                .unwrap_or("<no assistant message>");
            println!("{assistant}");
        }
    }

    Ok(())
}

async fn run_tui(
    cli: &Cli,
    store: Arc<FileSessionStore>,
    config_store: &PermissionConfigStore,
    hook_config_store: &HookConfigStore,
    provider_config_store: &ProviderConfigStore,
    cli_overrides: &PersistentPermissionConfig,
    cli_hook_overrides: &PersistentHookConfig,
    cwd: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: Option<SessionId>,
) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("`zetta tui` requires an interactive terminal (TTY)");
    }

    let session_id = session_id.unwrap_or_default();
    let session = store.load(&session_id).await?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Show)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;
    let _guard = TuiTerminalGuard;

    let runtime = Arc::new(Mutex::new(TuiRuntime::new(
        TuiState {
            session_id,
            active_provider: cli.provider.clone(),
            permission_mode: cli.permission_mode,
            focus: TuiFocus::Prompt,
            help_overlay: false,
            input: String::new(),
            input_history: tui_input_history_from_session(session.as_ref()),
            input_history_index: None,
            input_history_draft: None,
            pending_assistant: String::new(),
            activity_entries: vec![TuiActivityEntry {
                badge: "HELP".to_string(),
                title: "Tab focus • ? help • Enter submit • Shift+Enter newline".to_string(),
                detail: Some(
                    "Focus a pane, then use ↑/↓ to scroll • Alt+P/N recalls prompt history"
                        .to_string(),
                ),
                tone: ActivityTone::Neutral,
            }],
            last_error: None,
            busy: false,
            session,
            transcript_scroll: 0,
            activity_scroll: 0,
        },
        terminal,
    )));

    runtime.lock().expect("tui runtime").render()?;

    loop {
        if event::poll(Duration::from_millis(50))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };

            match handle_tui_key(runtime.clone(), key)? {
                TuiAction::None => {}
                TuiAction::Exit => break,
                TuiAction::Submit => {
                    run_tui_turn(
                        cli,
                        store.clone(),
                        config_store,
                        hook_config_store,
                        provider_config_store,
                        cli_overrides,
                        cli_hook_overrides,
                        cwd,
                        workspace_root,
                        runtime.clone(),
                    )
                    .await?;
                }
            }
        }
    }

    Ok(())
}

async fn run_tui_turn(
    cli: &Cli,
    store: Arc<FileSessionStore>,
    config_store: &PermissionConfigStore,
    hook_config_store: &HookConfigStore,
    provider_config_store: &ProviderConfigStore,
    cli_overrides: &PersistentPermissionConfig,
    cli_hook_overrides: &PersistentHookConfig,
    cwd: &std::path::Path,
    workspace_root: &std::path::Path,
    runtime: Arc<Mutex<TuiRuntime>>,
) -> Result<()> {
    let (session_id, active_provider, permission_mode, prompt) = {
        let mut runtime = runtime.lock().expect("tui runtime");
        let prompt = runtime.state.input.trim().to_string();
        runtime.record_input_history(&prompt);
        runtime.state.input.clear();
        runtime.state.pending_assistant.clear();
        runtime.state.last_error = None;
        runtime.state.busy = true;
        runtime.clear_history_navigation();
        runtime.push_event_line(format!("[submit] {}", summarize_history_content(&prompt)));
        runtime.render()?;
        (
            runtime.state.session_id,
            runtime.state.active_provider.clone(),
            runtime.state.permission_mode,
            prompt,
        )
    };

    let effective_overrides = effective_permission_overrides(cli_overrides, permission_mode);
    let engine = build_agent_engine(
        cli,
        store,
        config_store,
        hook_config_store,
        provider_config_store,
        &effective_overrides,
        cli_hook_overrides,
        cwd,
        workspace_root,
        Some(session_id),
        active_provider.as_deref(),
    )?;

    let request = TurnRequest {
        session_id: Some(session_id),
        prompt,
    };
    let mut model_sink = TuiModelStreamSink {
        runtime: runtime.clone(),
    };
    let mut event_sink = TuiEventSink {
        runtime: runtime.clone(),
    };

    let output = engine
        .run_turn_with_sinks(request, Some(&mut model_sink), Some(&mut event_sink))
        .await;

    let mut runtime = runtime.lock().expect("tui runtime");
    runtime.state.busy = false;
    runtime.state.pending_assistant.clear();

    match output {
        Ok(output) => {
            runtime.state.session_id = output.session.session_id;
            runtime.state.session = Some(output.session);
            for failure in output.hook_failures {
                runtime.push_activity_entry(TuiActivityEntry {
                    badge: "HOOK".to_string(),
                    title: format!("{} failed", failure.handler_name),
                    detail: Some(failure.error),
                    tone: ActivityTone::Warning,
                });
            }
        }
        Err(error) => {
            let message = render_cli_error_lines(&error).join(" | ");
            runtime.state.last_error = Some(message.clone());
            runtime.push_activity_entry(TuiActivityEntry {
                badge: "ERR".to_string(),
                title: "model request failed".to_string(),
                detail: Some(message),
                tone: ActivityTone::Error,
            });
        }
    }

    runtime.render()?;
    Ok(())
}

enum TuiAction {
    None,
    Submit,
    Exit,
}

fn handle_tui_key(runtime: Arc<Mutex<TuiRuntime>>, key: KeyEvent) -> Result<TuiAction> {
    let mut runtime = runtime.lock().expect("tui runtime");

    if runtime.state.help_overlay {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::F(1) => {
                runtime.state.help_overlay = false;
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            KeyCode::Tab => {
                runtime.cycle_focus(key.modifiers.contains(KeyModifiers::SHIFT));
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            _ => {
                runtime.render()?;
                return Ok(TuiAction::None);
            }
        }
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => return Ok(TuiAction::Exit),
            KeyCode::Char('j') => {
                runtime.state.input.push('\n');
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            KeyCode::Char('l') => {
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            KeyCode::Char('u') => {
                runtime.state.input.clear();
                runtime.clear_history_navigation();
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            KeyCode::Char('n') => {
                runtime.state.session_id = SessionId::new();
                runtime.state.session = None;
                runtime.state.focus = TuiFocus::Prompt;
                runtime.state.input_history.clear();
                runtime.state.input_history_index = None;
                runtime.state.input_history_draft = None;
                runtime.state.pending_assistant.clear();
                runtime.reset_scrolls();
                let session_id = runtime.state.session_id;
                runtime.push_event_line(format!("[session] switched to {session_id}"));
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            _ => {}
        }
    }

    if key.modifiers.contains(KeyModifiers::ALT) {
        match key.code {
            KeyCode::Char('p') => {
                if runtime.state.focus == TuiFocus::Prompt {
                    runtime.history_previous();
                }
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            KeyCode::Char('n') => {
                if runtime.state.focus == TuiFocus::Prompt {
                    runtime.history_next();
                }
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => return Ok(TuiAction::Exit),
        KeyCode::BackTab => {
            runtime.cycle_focus(true);
        }
        KeyCode::Tab if key.modifiers.is_empty() => {
            runtime.cycle_focus(false);
        }
        KeyCode::Char('?')
            if key.modifiers.is_empty()
                && (runtime.state.focus != TuiFocus::Prompt || runtime.state.input.is_empty()) =>
        {
            runtime.state.help_overlay = true;
        }
        KeyCode::Up if key.modifiers.is_empty() && runtime.state.focus != TuiFocus::Prompt => {
            runtime.scroll_current_focus(8);
        }
        KeyCode::Down if key.modifiers.is_empty() && runtime.state.focus != TuiFocus::Prompt => {
            runtime.scroll_current_focus(-8);
        }
        KeyCode::F(1) => {
            runtime.state.help_overlay = true;
        }
        KeyCode::Backspace => {
            if runtime.state.focus == TuiFocus::Prompt && !runtime.state.busy {
                runtime.state.input.pop();
                runtime.clear_history_navigation();
            }
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if runtime.state.focus == TuiFocus::Prompt && !runtime.state.busy {
                runtime.state.input.push('\n');
                runtime.clear_history_navigation();
            }
        }
        KeyCode::Enter => {
            let should_submit = runtime.state.focus == TuiFocus::Prompt
                && !runtime.state.busy
                && !runtime.state.input.trim().is_empty();
            runtime.render()?;
            return Ok(if should_submit {
                TuiAction::Submit
            } else {
                TuiAction::None
            });
        }
        KeyCode::Tab => {
            if runtime.state.focus == TuiFocus::Prompt && !runtime.state.busy {
                runtime.state.input.push_str("    ");
                runtime.clear_history_navigation();
            }
        }
        KeyCode::Char(character) => {
            if runtime.state.focus == TuiFocus::Prompt
                && !runtime.state.busy
                && !key.modifiers.contains(KeyModifiers::CONTROL)
            {
                runtime.state.input.push(character);
                runtime.clear_history_navigation();
            }
        }
        _ => {}
    }

    runtime.render()?;
    Ok(TuiAction::None)
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

fn parse_repl_command(input: &str) -> Option<Result<ReplCommand, String>> {
    let trimmed = input.trim();
    let mut parts = trimmed.split_whitespace();
    let command = parts.next()?;

    let parsed = match command {
        ":help" => Ok(ReplCommand::Help),
        ":exit" | ":quit" => Ok(ReplCommand::Exit),
        ":session" => Ok(ReplCommand::Session),
        ":tools" => Ok(ReplCommand::Tools),
        ":history" => Ok(ReplCommand::History),
        ":search" => {
            let query = trimmed[":search".len()..].trim();
            if query.is_empty() {
                return Some(Err("expected `:search <text>`".to_string()));
            }
            Ok(ReplCommand::Search(query.to_string()))
        }
        ":last" => Ok(ReplCommand::Last),
        ":write" => {
            let path = trimmed[":write".len()..].trim();
            if path.is_empty() {
                return Some(Err("expected `:write <path>`".to_string()));
            }
            Ok(ReplCommand::Write(path.to_string()))
        }
        ":show" => Ok(ReplCommand::Show),
        ":new" => Ok(ReplCommand::New),
        ":reset" => Ok(ReplCommand::Reset),
        ":trim" => {
            let value = trimmed[":trim".len()..].trim();
            if value.is_empty() {
                return Some(Err("expected `:trim <turns>`".to_string()));
            }
            match value.parse::<usize>() {
                Ok(turns) if turns > 0 => Ok(ReplCommand::Trim(turns)),
                Ok(_) => Err("invalid trim value `0`; expected a positive integer".to_string()),
                Err(error) => Err(format!("invalid trim value `{value}`: {error}")),
            }
        }
        ":retry" => Ok(ReplCommand::Retry),
        ":rerun" => {
            let value = trimmed[":rerun".len()..].trim();
            if value.is_empty() {
                return Some(Err("expected `:rerun <turns_back>`".to_string()));
            }
            match value.parse::<usize>() {
                Ok(turns_back) if turns_back > 0 => Ok(ReplCommand::Rerun(turns_back)),
                Ok(_) => Err("invalid rerun value `0`; expected a positive integer".to_string()),
                Err(error) => Err(format!("invalid rerun value `{value}`: {error}")),
            }
        }
        ":export" => {
            let path = trimmed[":export".len()..].trim();
            if path.is_empty() {
                return Some(Err("expected `:export <path>`".to_string()));
            }
            Ok(ReplCommand::Export(path.to_string()))
        }
        ":provider" => match parts.next() {
            None => Ok(ReplCommand::Provider),
            Some("use") => {
                let Some(provider_name) = parts.next() else {
                    return Some(Err("expected `:provider use <name>`".to_string()));
                };
                if parts.next().is_some() {
                    return Some(Err("expected only one provider name after `:provider use`".to_string()));
                }
                Ok(ReplCommand::ProviderUse(provider_name.to_string()))
            }
            Some("clear") => {
                if parts.next().is_some() {
                    return Some(Err("expected `:provider clear` with no extra arguments".to_string()));
                }
                Ok(ReplCommand::ProviderClear)
            }
            Some(other) => Err(format!(
                "unknown provider subcommand `{other}`; use `:provider`, `:provider use <name>`, or `:provider clear`"
            )),
        },
        ":config" => Ok(ReplCommand::Config),
        ":overview" => Ok(ReplCommand::Overview),
        ":fork" => Ok(ReplCommand::Fork),
        ":mode" => match parts.next() {
            None => Ok(ReplCommand::ModeShow),
            Some(mode) => {
                if parts.next().is_some() {
                    return Some(Err("expected `:mode <read-only|workspace-write|bypass-permissions>`".to_string()));
                }
                parse_repl_permission_mode(mode).map(ReplCommand::ModeSet)
            }
        },
        ":ui" => match parts.next() {
            None => Ok(ReplCommand::UiShow),
            Some(mode) => {
                if parts.next().is_some() {
                    return Some(Err("expected `:ui <off|pretty|json>`".to_string()));
                }
                parse_repl_ui_mode(mode).map(ReplCommand::UiSet)
            }
        },
        ":events" => match parts.next() {
            None => Ok(ReplCommand::EventsShow),
            Some(value) => {
                if parts.next().is_some() {
                    return Some(Err("expected `:events on` or `:events off`".to_string()));
                }
                parse_repl_toggle(value).map(ReplCommand::EventsSet)
            }
        },
        ":json" => match parts.next() {
            None => Ok(ReplCommand::JsonShow),
            Some(value) => {
                if parts.next().is_some() {
                    return Some(Err("expected `:json on` or `:json off`".to_string()));
                }
                parse_repl_toggle(value).map(ReplCommand::JsonSet)
            }
        },
        ":load" => {
            let Some(session_id) = parts.next() else {
                return Some(Err("expected `:load <session_id>`".to_string()));
            };
            if parts.next().is_some() {
                return Some(Err("expected only one session id after `:load`".to_string()));
            }
            session_id
                .parse::<SessionId>()
                .map(ReplCommand::Load)
                .map_err(|error| format!("invalid session id `{session_id}`: {error}"))
        }
        _ => return None,
    };

    Some(parsed)
}

fn parse_repl_permission_mode(input: &str) -> Result<CliPermissionMode, String> {
    match input {
        "read-only" => Ok(CliPermissionMode::ReadOnly),
        "workspace-write" => Ok(CliPermissionMode::WorkspaceWrite),
        "bypass-permissions" => Ok(CliPermissionMode::BypassPermissions),
        _ => Err(format!(
            "invalid mode `{input}`; expected `read-only`, `workspace-write`, or `bypass-permissions`"
        )),
    }
}

fn parse_repl_toggle(input: &str) -> Result<bool, String> {
    match input {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("invalid toggle `{input}`; expected `on` or `off`")),
    }
}

fn parse_repl_ui_mode(input: &str) -> Result<CliUiMode, String> {
    match input {
        "off" => Ok(CliUiMode::Off),
        "pretty" => Ok(CliUiMode::Pretty),
        "json" => Ok(CliUiMode::Json),
        _ => Err(format!(
            "invalid ui mode `{input}`; expected `off`, `pretty`, or `json`"
        )),
    }
}

fn render_repl_prompt(
    session_id: SessionId,
    active_provider: Option<&str>,
    permission_mode: Option<CliPermissionMode>,
) -> String {
    let short_session = short_session_id(&session_id.to_string());
    let mode = permission_mode
        .unwrap_or(CliPermissionMode::WorkspaceWrite)
        .short_label();
    match active_provider {
        Some(provider) => format!("zetta[{short_session} {mode} {provider}]> "),
        None => format!("zetta[{short_session} {mode}]> "),
    }
}

fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn print_session_summary(session: &zetta_protocol::SessionSnapshot) {
    println!("session_id: {}", session.session_id);
    println!("messages: {}", session.messages.len());
    if !session.tags.is_empty() {
        println!("tags: {}", session.tags.join(", "));
    }
    if !session.metadata.is_empty() {
        println!(
            "metadata: {}",
            session
                .metadata
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if session.messages.is_empty() {
        println!("<empty session>");
        return;
    }

    for message in &session.messages {
        let role = match message.role {
            zetta_protocol::MessageRole::System => "system",
            zetta_protocol::MessageRole::User => "user",
            zetta_protocol::MessageRole::Assistant => "assistant",
            zetta_protocol::MessageRole::Tool => "tool",
        };
        println!("[{role}] {}", message.content);
    }
}

fn latest_assistant_message(session: &zetta_protocol::SessionSnapshot) -> Option<&str> {
    session
        .messages
        .iter()
        .rev()
        .find(|message| matches!(message.role, zetta_protocol::MessageRole::Assistant))
        .map(|message| message.content.as_str())
}

fn print_session_history(session: &zetta_protocol::SessionSnapshot) {
    println!("session_id: {}", session.session_id);
    println!("messages: {}", session.messages.len());
    if session.messages.is_empty() {
        println!("<empty session>");
        return;
    }

    for (index, message) in session.messages.iter().enumerate() {
        let role = match message.role {
            zetta_protocol::MessageRole::System => "system",
            zetta_protocol::MessageRole::User => "user",
            zetta_protocol::MessageRole::Assistant => "assistant",
            zetta_protocol::MessageRole::Tool => "tool",
        };
        println!(
            "{:>3}. [{}] {}",
            index + 1,
            role,
            summarize_history_content(&message.content)
        );
    }
}

fn print_session_search_results(session: &zetta_protocol::SessionSnapshot, query: &str) {
    println!("session_id: {}", session.session_id);
    println!("search: {query}");
    let matches = search_session_messages(session, query);
    if matches.is_empty() {
        println!("<no matches>");
        return;
    }

    for (index, role, content) in matches {
        println!("{:>3}. [{}] {}", index + 1, role, content);
    }
}

fn search_session_messages(
    session: &zetta_protocol::SessionSnapshot,
    query: &str,
) -> Vec<(usize, &'static str, String)> {
    let query = query.to_ascii_lowercase();
    session
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.content.to_ascii_lowercase().contains(&query))
        .map(|(index, message)| {
            let role = match message.role {
                zetta_protocol::MessageRole::System => "system",
                zetta_protocol::MessageRole::User => "user",
                zetta_protocol::MessageRole::Assistant => "assistant",
                zetta_protocol::MessageRole::Tool => "tool",
            };
            (index, role, summarize_history_content(&message.content))
        })
        .collect()
}

fn summarize_history_content(content: &str) -> String {
    const MAX_LEN: usize = 100;

    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut summary = normalized.chars().take(MAX_LEN).collect::<String>();
    if normalized.chars().count() > MAX_LEN {
        summary.push_str("...");
    }
    summary
}

fn user_turn_from_end(
    session: &zetta_protocol::SessionSnapshot,
    turns_back: usize,
) -> Option<(usize, String)> {
    if turns_back == 0 {
        return None;
    }

    session
        .messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| matches!(message.role, zetta_protocol::MessageRole::User))
        .nth(turns_back - 1)
        .map(|(index, message)| (index, message.content.clone()))
}

fn trim_session_to_last_user_turns(
    session: &mut zetta_protocol::SessionSnapshot,
    turns_to_keep: usize,
) -> usize {
    if turns_to_keep == 0 {
        return 0;
    }

    let user_indices = session
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            matches!(message.role, zetta_protocol::MessageRole::User).then_some(index)
        })
        .collect::<Vec<_>>();

    if user_indices.len() <= turns_to_keep {
        return 0;
    }

    let keep_from = user_indices[user_indices.len() - turns_to_keep];
    let removed = keep_from;
    session.messages = session.messages.split_off(keep_from);
    removed
}

fn print_provider_summary(
    active_provider: Option<&str>,
    provider_config_store: &ProviderConfigStore,
) -> Result<()> {
    match resolve_provider_profile_by_name(active_provider, provider_config_store)? {
        Some(profile) => {
            let provider_name = active_provider.unwrap_or("<unnamed>");
            println!("provider: {provider_name}");
            println!("{}", serde_json::to_string_pretty(&profile)?);
        }
        None => println!("provider: <none>"),
    }
    Ok(())
}

fn print_runtime_summary(
    cli: &Cli,
    config_store: &PermissionConfigStore,
    cli_overrides: &PersistentPermissionConfig,
    cwd: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: SessionId,
    active_provider: Option<&str>,
    permission_mode_override: Option<CliPermissionMode>,
    ui_mode: CliUiMode,
) -> Result<()> {
    let provider_config_store = ProviderConfigStore::new(&cli.config_dir);
    let provider_profile =
        resolve_provider_profile_by_name(active_provider, &provider_config_store)?;
    let resolved = resolve_openai_options(cli, provider_profile.as_ref());
    let tool_context = build_tool_context(
        &effective_permission_overrides(cli_overrides, permission_mode_override),
        config_store,
        cwd,
        workspace_root,
        Some(session_id),
    )?;
    let registry = build_registry();
    let visible_tools = registry.visible_names(&tool_context);
    let policy = tool_context.permissions();

    println!("session_id: {session_id}");
    println!("cwd: {}", cwd.display());
    println!("workspace_root: {}", policy.workspace_root().display());
    println!("permission_mode: {:?}", policy.mode());
    println!("config_dir: {}", cli.config_dir.display());
    println!("session_dir: {}", cli.session_dir.display());
    println!("stream_output: {}", cli.stream_output);
    println!("ui_mode: {}", ui_mode.as_str());
    println!(
        "events: {}",
        if ui_mode == CliUiMode::Pretty {
            "on"
        } else {
            "off"
        }
    );
    println!(
        "json: {}",
        if ui_mode == CliUiMode::Json {
            "on"
        } else {
            "off"
        }
    );
    println!("provider: {}", active_provider.unwrap_or("<none>"));
    println!(
        "model_driver: {}",
        if provider_profile.is_some() {
            "openai-compatible"
        } else {
            match cli.model_driver {
                CliModelDriver::RuleBased => "rule-based",
                CliModelDriver::OpenaiCompatible => "openai-compatible",
            }
        }
    );
    println!("api_key_env: {}", resolved.api_key_env);
    println!(
        "model_name: {}",
        resolved.model_name.as_deref().unwrap_or("<unset>")
    );
    println!(
        "api_base: {}",
        resolved.api_base.as_deref().unwrap_or("<default>")
    );
    println!(
        "readable_roots: {}",
        policy
            .readable_roots()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "writable_roots: {}",
        policy
            .writable_roots()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("visible_tools: {}", visible_tools.join(", "));
    Ok(())
}

struct StderrTurnPresenter {
    mode: CliUiMode,
    active_provider: Option<String>,
    stream_output: bool,
    prompt_preview: String,
    started_at: Instant,
    requested_tools: usize,
    completed_tools: usize,
    denied_tools: usize,
    failed_tools: usize,
}

impl StderrTurnPresenter {
    fn new(
        mode: CliUiMode,
        active_provider: Option<&str>,
        stream_output: bool,
        prompt: &str,
    ) -> Self {
        Self {
            mode,
            active_provider: active_provider.map(ToString::to_string),
            stream_output,
            prompt_preview: summarize_history_content(prompt),
            started_at: Instant::now(),
            requested_tools: 0,
            completed_tools: 0,
            denied_tools: 0,
            failed_tools: 0,
        }
    }

    fn print_pretty_event(&mut self, event: &zetta_protocol::EngineEvent) {
        match event {
            zetta_protocol::EngineEvent::SessionLoaded { session_id, is_new } => {
                match self.active_provider.as_deref() {
                    Some(provider) => eprintln!(
                        "[turn] session={} state={} provider={} prompt=\"{}\"",
                        session_id,
                        if *is_new { "new" } else { "resume" },
                        provider,
                        self.prompt_preview
                    ),
                    None => eprintln!(
                        "[turn] session={} state={} prompt=\"{}\"",
                        session_id,
                        if *is_new { "new" } else { "resume" },
                        self.prompt_preview
                    ),
                }
            }
            zetta_protocol::EngineEvent::UserMessagePersisted { .. } => {
                eprintln!("[turn] user message persisted");
            }
            zetta_protocol::EngineEvent::ToolCallRequested { call } => {
                self.requested_tools += 1;
                eprintln!(
                    "[tool] request {} {}",
                    call.name,
                    summarize_json_inline(&call.input, 88)
                );
            }
            zetta_protocol::EngineEvent::ToolCallDenied { call, reason } => {
                self.denied_tools += 1;
                eprintln!("[tool] denied {}: {reason}", call.name);
            }
            zetta_protocol::EngineEvent::ToolCallFailed { call, error } => {
                self.failed_tools += 1;
                eprintln!("[tool] failed {}: {error}", call.name);
            }
            zetta_protocol::EngineEvent::ToolCallCompleted { result } => {
                self.completed_tools += 1;
                eprintln!(
                    "[tool] done {} {}",
                    result.name,
                    summarize_json_inline(&result.output, 88)
                );
            }
            zetta_protocol::EngineEvent::AssistantMessagePersisted { message } => {
                if !self.stream_output {
                    eprintln!(
                        "[assistant] {}",
                        summarize_history_content(&message.content)
                    );
                } else {
                    eprintln!("[assistant] response persisted");
                }
            }
            zetta_protocol::EngineEvent::TurnFinished { session_id } => {
                eprintln!(
                    "[summary] session={} tools={}/{}/{}/{} elapsed={}ms",
                    session_id,
                    self.requested_tools,
                    self.completed_tools,
                    self.denied_tools,
                    self.failed_tools,
                    self.started_at.elapsed().as_millis()
                );
            }
        }
    }
}

impl EngineEventSink for StderrTurnPresenter {
    fn on_event(&mut self, event: &zetta_protocol::EngineEvent) -> Result<()> {
        match self.mode {
            CliUiMode::Off => Ok(()),
            CliUiMode::Pretty => {
                self.print_pretty_event(event);
                Ok(())
            }
            CliUiMode::Json => {
                eprintln!("{}", serde_json::to_string(event)?);
                Ok(())
            }
        }
    }
}

struct TuiTerminalGuard;

impl Drop for TuiTerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, Show);
    }
}

struct TuiState {
    session_id: SessionId,
    active_provider: Option<String>,
    permission_mode: Option<CliPermissionMode>,
    focus: TuiFocus,
    help_overlay: bool,
    input: String,
    input_history: Vec<String>,
    input_history_index: Option<usize>,
    input_history_draft: Option<String>,
    pending_assistant: String,
    activity_entries: Vec<TuiActivityEntry>,
    last_error: Option<String>,
    busy: bool,
    session: Option<zetta_protocol::SessionSnapshot>,
    transcript_scroll: usize,
    activity_scroll: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TuiFocus {
    Conversation,
    Activity,
    Prompt,
}

#[derive(Clone, Debug)]
struct StyledTextLine {
    text: String,
    style: Style,
}

impl StyledTextLine {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

#[derive(Clone, Debug)]
struct TuiActivityEntry {
    badge: String,
    title: String,
    detail: Option<String>,
    tone: ActivityTone,
}

#[derive(Clone, Copy, Debug)]
enum ActivityTone {
    Neutral,
    Running,
    Success,
    Warning,
    Error,
    Assistant,
}

impl ActivityTone {
    fn title_style(self) -> Style {
        match self {
            Self::Neutral => Style::default().fg(Color::Gray),
            Self::Running => Style::default().fg(Color::LightCyan),
            Self::Success => Style::default().fg(Color::LightGreen),
            Self::Warning => Style::default().fg(Color::Yellow),
            Self::Error => Style::default().fg(Color::LightRed),
            Self::Assistant => Style::default().fg(Color::White),
        }
    }

    fn detail_style(self) -> Style {
        match self {
            Self::Error => Style::default().fg(Color::Red),
            Self::Warning => Style::default().fg(Color::DarkGray),
            Self::Running => Style::default().fg(Color::DarkGray),
            Self::Success => Style::default().fg(Color::DarkGray),
            Self::Assistant => subtle_text(Color::DarkGray),
            Self::Neutral => subtle_text(Color::DarkGray),
        }
    }
}

struct TuiRuntime {
    state: TuiState,
    terminal: TuiTerminal,
}

impl TuiRuntime {
    fn new(state: TuiState, terminal: TuiTerminal) -> Self {
        Self { state, terminal }
    }

    fn cycle_focus(&mut self, reverse: bool) {
        self.state.focus = match (self.state.focus, reverse) {
            (TuiFocus::Conversation, false) => TuiFocus::Activity,
            (TuiFocus::Activity, false) => TuiFocus::Prompt,
            (TuiFocus::Prompt, false) => TuiFocus::Conversation,
            (TuiFocus::Conversation, true) => TuiFocus::Prompt,
            (TuiFocus::Activity, true) => TuiFocus::Conversation,
            (TuiFocus::Prompt, true) => TuiFocus::Activity,
        };
    }

    fn push_activity_entry(&mut self, entry: TuiActivityEntry) {
        self.state.activity_entries.push(entry);
        const MAX_ACTIVITY_ENTRIES: usize = 200;
        if self.state.activity_entries.len() > MAX_ACTIVITY_ENTRIES {
            let trim = self.state.activity_entries.len() - MAX_ACTIVITY_ENTRIES;
            self.state.activity_entries.drain(0..trim);
        }
    }

    fn push_event_line(&mut self, line: String) {
        self.push_activity_entry(TuiActivityEntry {
            badge: "NOTE".to_string(),
            title: line,
            detail: None,
            tone: ActivityTone::Neutral,
        });
    }

    fn record_input_history(&mut self, input: &str) {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return;
        }

        if self
            .state
            .input_history
            .last()
            .is_some_and(|last| last == trimmed)
        {
            self.state.input_history_index = None;
            self.state.input_history_draft = None;
            return;
        }

        self.state.input_history.push(trimmed.to_string());
        const MAX_INPUT_HISTORY: usize = 100;
        if self.state.input_history.len() > MAX_INPUT_HISTORY {
            let trim = self.state.input_history.len() - MAX_INPUT_HISTORY;
            self.state.input_history.drain(0..trim);
        }
        self.state.input_history_index = None;
        self.state.input_history_draft = None;
    }

    fn history_previous(&mut self) {
        if self.state.input_history.is_empty() {
            return;
        }

        let next_index = match self.state.input_history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.state.input_history_draft = Some(self.state.input.clone());
                self.state.input_history.len().saturating_sub(1)
            }
        };
        self.state.input_history_index = Some(next_index);
        self.state.input = self.state.input_history[next_index].clone();
    }

    fn history_next(&mut self) {
        let Some(index) = self.state.input_history_index else {
            return;
        };

        if index + 1 < self.state.input_history.len() {
            let next_index = index + 1;
            self.state.input_history_index = Some(next_index);
            self.state.input = self.state.input_history[next_index].clone();
        } else {
            self.state.input_history_index = None;
            self.state.input = self.state.input_history_draft.take().unwrap_or_default();
        }
    }

    fn clear_history_navigation(&mut self) {
        self.state.input_history_index = None;
        self.state.input_history_draft = None;
    }

    fn scroll_current_focus(&mut self, delta: isize) {
        match self.state.focus {
            TuiFocus::Conversation => self.scroll_transcript(delta),
            TuiFocus::Activity => self.scroll_activity(delta),
            TuiFocus::Prompt => {}
        }
    }

    fn scroll_transcript(&mut self, delta: isize) {
        self.state.transcript_scroll = apply_scroll_delta(self.state.transcript_scroll, delta);
    }

    fn scroll_activity(&mut self, delta: isize) {
        self.state.activity_scroll = apply_scroll_delta(self.state.activity_scroll, delta);
    }

    fn reset_scrolls(&mut self) {
        self.state.transcript_scroll = 0;
        self.state.activity_scroll = 0;
    }

    fn render(&mut self) -> Result<()> {
        let state = &self.state;
        self.terminal.draw(|frame| render_tui_frame(frame, state))?;
        Ok(())
    }
}

fn apply_scroll_delta(current: usize, delta: isize) -> usize {
    if delta >= 0 {
        current.saturating_add(delta as usize)
    } else {
        current.saturating_sub(delta.unsigned_abs())
    }
}

fn tui_input_history_from_session(
    session: Option<&zetta_protocol::SessionSnapshot>,
) -> Vec<String> {
    session
        .into_iter()
        .flat_map(|session| session.messages.iter())
        .filter(|message| matches!(message.role, zetta_protocol::MessageRole::User))
        .map(|message| message.content.trim())
        .filter(|content| !content.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn pane_title(base: &str, scroll: usize) -> String {
    let _ = scroll;
    format!(" {base} ")
}

fn panel_block(title: String, color: Color, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(if focused { color } else { Color::DarkGray }))
        .title(Span::styled(
            title,
            Style::default().fg(color).add_modifier(if focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ))
}

fn subtle_text(color: Color) -> Style {
    Style::default().fg(color)
}

fn transcript_role_style(role: zetta_protocol::MessageRole) -> (Style, Style) {
    match role {
        zetta_protocol::MessageRole::System => (
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
            subtle_text(Color::Gray),
        ),
        zetta_protocol::MessageRole::User => (
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Rgb(228, 232, 238)),
        ),
        zetta_protocol::MessageRole::Assistant => (
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::White),
        ),
        zetta_protocol::MessageRole::Tool => (
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            subtle_text(Color::DarkGray),
        ),
    }
}

fn render_tui_frame(frame: &mut ratatui::Frame<'_>, state: &TuiState) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(TUI_COMPOSER_HEIGHT),
        ])
        .split(area);

    render_tui_header(frame, vertical[0], state);

    let body = vertical[1];
    let body_chunks = if body.width >= 96 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(48), Constraint::Length(32)])
            .split(body)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(11)])
            .split(body)
    };

    let transcript_area = body_chunks[0];
    let side_area = body_chunks[1];
    let side_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(4)])
        .split(side_area);

    render_transcript_pane(frame, transcript_area, state);
    render_overview_pane(frame, side_chunks[0], state);
    render_activity_pane(frame, side_chunks[1], state);
    render_composer_pane(frame, vertical[2], state);

    if state.help_overlay {
        render_help_overlay(frame, area);
    }
}

fn render_tui_header(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let provider = state.active_provider.as_deref().unwrap_or("placeholder");
    let mode = state
        .permission_mode
        .unwrap_or(CliPermissionMode::WorkspaceWrite)
        .as_str();
    let state_label = if state.busy { "RUNNING" } else { "IDLE" };
    let state_style = if state.busy {
        Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let session = state.session_id.to_string();
    let session_short = short_session_id(&session);

    let line = Line::from(vec![
        Span::styled(
            " Zetta ",
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(58, 96, 148))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            subtle_text(Color::DarkGray),
        ),
        Span::raw("   "),
        Span::styled("provider", subtle_text(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(provider.to_string(), Style::default().fg(Color::Gray)),
        Span::raw("   "),
        Span::styled("mode", subtle_text(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(mode.to_string(), Style::default().fg(Color::Gray)),
        Span::raw("   "),
        Span::styled("session", subtle_text(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(session_short, Style::default().fg(Color::LightYellow)),
        Span::raw("   "),
        Span::styled("focus", subtle_text(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(
            match state.focus {
                TuiFocus::Conversation => "conversation",
                TuiFocus::Activity => "activity",
                TuiFocus::Prompt => "prompt",
            },
            Style::default().fg(Color::Gray),
        ),
        Span::raw("   "),
        Span::styled(state_label, state_style),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn render_transcript_pane(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let block = panel_block(
        pane_title("Conversation", state.transcript_scroll),
        if state.busy {
            Color::LightGreen
        } else {
            Color::Gray
        },
        state.focus == TuiFocus::Conversation,
    );
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;
    let wrapped = wrap_styled_lines(&tui_transcript_lines(state), inner_width.max(1));
    let lines = scrolled_wrapped_lines(&wrapped, inner_height.max(1), state.transcript_scroll);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn render_overview_pane(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let block = panel_block(" Overview ".to_string(), Color::Gray, false);
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;
    let wrapped = wrap_styled_lines(&tui_overview_lines(state), inner_width.max(1));
    let lines = scrolled_wrapped_lines(&wrapped, inner_height.max(1), 0);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn render_activity_pane(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let block = panel_block(
        pane_title("Activity", state.activity_scroll),
        Color::Gray,
        state.focus == TuiFocus::Activity,
    );
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;
    let wrapped = wrap_styled_lines(&tui_activity_lines(state), inner_width.max(1));
    let lines = scrolled_wrapped_lines(&wrapped, inner_height.max(1), state.activity_scroll);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn render_help_overlay(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let overlay = centered_rect(area, 72, 12);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(Color::Gray))
        .title(Span::styled(
            " Help ",
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let lines = vec![
        Line::from(Span::styled(
            "Tab / Shift+Tab    switch focus between Conversation, Activity, and Prompt",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "Up / Down          scroll the focused pane when Conversation or Activity is active",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "Enter              submit the current prompt when Prompt is focused",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "Shift+Enter        insert a newline in the Prompt composer",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "Alt+P / Alt+N      cycle through prior submitted prompts in Prompt focus",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "Ctrl+N / Ctrl+U    new session / clear prompt",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "? / F1 / Esc       open, close, or dismiss this help overlay",
            Style::default().fg(Color::White),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let overlay_width = width.min(area.width.saturating_sub(2)).max(10);
    let overlay_height = height.min(area.height.saturating_sub(2)).max(6);
    let x = area
        .x
        .saturating_add(area.width.saturating_sub(overlay_width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(overlay_height) / 2);
    Rect::new(x, y, overlay_width, overlay_height)
}

fn render_composer_pane(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let title = if state.busy {
        " Prompt • sending "
    } else {
        " Prompt "
    };
    let block = panel_block(
        title.to_string(),
        if state.busy {
            Color::LightGreen
        } else {
            Color::Gray
        },
        state.focus == TuiFocus::Prompt,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let input_area = sections[0];
    let hint_area = sections[1];
    let inner_width = input_area.width as usize;
    let inner_height = input_area.height as usize;
    let (lines, cursor_line, cursor_col) =
        composer_display_lines(state, inner_width.max(1), inner_height.max(1));
    frame.render_widget(Paragraph::new(Text::from(lines)), input_area);

    let composer_hint = if state.active_provider.is_none() {
        "Tab focus • ? help • Enter submit • Shift+Enter newline • placeholder model"
    } else {
        "Tab focus • ? help • Enter submit • Shift+Enter newline • Alt+P/N history"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            composer_hint,
            subtle_text(Color::DarkGray),
        ))),
        hint_area,
    );

    if !state.busy {
        frame.set_cursor_position((
            input_area.x.saturating_add(cursor_col as u16).min(
                input_area
                    .x
                    .saturating_add(input_area.width.saturating_sub(1)),
            ),
            input_area.y.saturating_add(cursor_line as u16).min(
                input_area
                    .y
                    .saturating_add(input_area.height.saturating_sub(1)),
            ),
        ));
    }
}

fn tui_transcript_lines(state: &TuiState) -> Vec<StyledTextLine> {
    let mut lines = Vec::new();

    if let Some(session) = &state.session {
        for message in &session.messages {
            let (header_style, body_style) = transcript_role_style(message.role.clone());
            let (label, prefix) = match message.role {
                zetta_protocol::MessageRole::System => ("System", "·"),
                zetta_protocol::MessageRole::User => ("You", ">"),
                zetta_protocol::MessageRole::Assistant => ("Zetta", "›"),
                zetta_protocol::MessageRole::Tool => ("Tool", "·"),
            };
            let content = if matches!(message.role, zetta_protocol::MessageRole::Tool) {
                summarize_tool_result(&message.content)
            } else {
                message.content.clone()
            };
            lines.push(StyledTextLine::styled(
                format!("{prefix} {label}"),
                header_style,
            ));
            for line in content.lines() {
                lines.push(StyledTextLine::styled(format!("  {line}"), body_style));
            }
            lines.push(StyledTextLine::plain(String::new()));
        }
    } else {
        lines.push(StyledTextLine::styled(
            "No messages yet. Start by asking Zetta to inspect or modify files in this workspace.",
            subtle_text(Color::DarkGray),
        ));
    }

    if !state.pending_assistant.is_empty() {
        lines.push(StyledTextLine::styled(
            "› Zetta",
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ));
        for line in state.pending_assistant.lines() {
            lines.push(StyledTextLine::styled(
                format!("  {line}"),
                Style::default().fg(Color::White),
            ));
        }
    }

    lines
}

fn tui_overview_lines(state: &TuiState) -> Vec<StyledTextLine> {
    let mut lines = vec![
        kv_line(
            "provider",
            state.active_provider.as_deref().unwrap_or("<none>"),
        ),
        kv_line(
            "mode",
            state
                .permission_mode
                .unwrap_or(CliPermissionMode::WorkspaceWrite)
                .as_str(),
        ),
        kv_line("state", if state.busy { "running" } else { "idle" }),
    ];

    if let Some(session) = &state.session {
        let overview = build_session_overview(session);
        lines.push(kv_line("turns", &overview.user_turns.to_string()));
        lines.push(kv_line("messages", &session.messages.len().to_string()));
        lines.push(kv_line(
            "tools",
            &format!(
                "ok={} deny={} fail={}",
                overview.completed_tools, overview.denied_tools, overview.failed_tools
            ),
        ));
    }

    if state.busy {
        if let Some(entry) = state
            .activity_entries
            .iter()
            .rev()
            .find(|entry| matches!(entry.tone, ActivityTone::Running))
        {
            lines.push(kv_line("active", &entry.title));
        } else {
            lines.push(kv_line("active", "planning"));
        }
    }

    if state.active_provider.is_none() {
        lines.push(StyledTextLine::plain(String::new()));
        lines.push(StyledTextLine::styled(
            "Tip: pass --provider <name> to use a real model instead of the local placeholder.",
            Style::default().fg(Color::Gray),
        ));
    }

    if let Some(error) = &state.last_error {
        lines.push(StyledTextLine::plain(String::new()));
        lines.push(StyledTextLine::styled(
            "Last error",
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(StyledTextLine::styled(
            error.clone(),
            Style::default().fg(Color::LightRed),
        ));
    }

    lines
}

fn tui_activity_lines(state: &TuiState) -> Vec<StyledTextLine> {
    let mut lines = Vec::new();
    if state.activity_entries.is_empty() {
        lines.push(StyledTextLine::styled(
            "No activity yet.",
            subtle_text(Color::DarkGray),
        ));
        return lines;
    }

    let entries = state
        .activity_entries
        .iter()
        .rev()
        .take(24)
        .collect::<Vec<_>>();
    let current_index = entries
        .iter()
        .position(|entry| matches!(entry.tone, ActivityTone::Running));

    if let Some(index) = current_index {
        let entry = entries[index];
        lines.push(StyledTextLine::styled(
            "Current",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(StyledTextLine::styled(
            format!("› [{}] {}", entry.badge.to_lowercase(), entry.title),
            entry.tone.title_style(),
        ));
        if let Some(detail) = &entry.detail {
            lines.push(StyledTextLine::styled(
                format!("  {}", detail),
                entry.tone.detail_style(),
            ));
        }
        lines.push(StyledTextLine::plain(String::new()));
    }

    let mut recent = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| (Some(index) != current_index).then_some(*entry))
        .take(12)
        .collect::<Vec<_>>();

    if !recent.is_empty() {
        lines.push(StyledTextLine::styled(
            "Recent",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ));
    }

    recent.reverse();
    for entry in recent {
        lines.push(StyledTextLine::styled(
            format!("· [{}] {}", entry.badge.to_lowercase(), entry.title),
            entry.tone.title_style(),
        ));
        if let Some(detail) = &entry.detail {
            lines.push(StyledTextLine::styled(
                format!("  {}", detail),
                entry.tone.detail_style(),
            ));
        }
        lines.push(StyledTextLine::plain(String::new()));
    }
    lines
}

fn composer_display_lines(
    state: &TuiState,
    width: usize,
    height: usize,
) -> (Vec<Line<'static>>, usize, usize) {
    if state.busy {
        return (
            vec![
                Line::from(Span::styled(
                    "Zetta is working on the current turn.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "Prompt editing is locked until the turn finishes.",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            0,
            0,
        );
    }

    if state.input.is_empty() {
        let provider = state.active_provider.as_deref().unwrap_or("placeholder");
        let mode = state
            .permission_mode
            .unwrap_or(CliPermissionMode::WorkspaceWrite)
            .as_str();
        return (
            vec![
                Line::from(Span::styled(
                    format!("Ask Zetta about this workspace. Provider: {provider} • Mode: {mode}"),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "Tab focuses panes • Enter sends • Shift+Enter newline • Alt+P/N recalls prior prompts",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            0,
            0,
        );
    }

    let wrapped = wrap_plain_lines(&split_text_lines(&state.input), width.max(1));
    let visible = take_tail(&wrapped, height.max(1));
    let cursor_line = visible.len().saturating_sub(1);
    let cursor_col = visible.last().map(|line| display_width(line)).unwrap_or(0);
    let rendered = visible
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::White))))
        .collect::<Vec<_>>();
    (rendered, cursor_line, cursor_col)
}

fn wrap_styled_lines(lines: &[StyledTextLine], width: usize) -> Vec<Line<'static>> {
    let mut wrapped = Vec::new();

    for line in lines {
        if line.text.is_empty() {
            wrapped.push(Line::from(Span::styled(String::new(), line.style)));
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in line.text.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width + ch_width > width && !current.is_empty() {
                wrapped.push(Line::from(Span::styled(current, line.style)));
                current = String::new();
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
            if current_width >= width && !current.is_empty() {
                wrapped.push(Line::from(Span::styled(current, line.style)));
                current = String::new();
                current_width = 0;
            }
        }
        if !current.is_empty() {
            wrapped.push(Line::from(Span::styled(current, line.style)));
        }
    }

    wrapped
}

fn scrolled_wrapped_lines(
    wrapped: &[Line<'static>],
    max_lines: usize,
    scroll: usize,
) -> Vec<Line<'static>> {
    let visible_end = wrapped.len().saturating_sub(scroll);
    let visible_start = visible_end.saturating_sub(max_lines.max(1));
    wrapped[visible_start..visible_end].to_vec()
}

fn wrap_plain_lines(lines: &[String], width: usize) -> Vec<String> {
    let styled = lines
        .iter()
        .cloned()
        .map(StyledTextLine::plain)
        .collect::<Vec<_>>();
    wrap_styled_lines(&styled, width.max(1))
        .into_iter()
        .map(|line| line.to_string())
        .collect()
}

fn take_tail<T: Clone>(items: &[T], max_items: usize) -> Vec<T> {
    let start = items.len().saturating_sub(max_items);
    items[start..].to_vec()
}

fn split_text_lines(input: &str) -> Vec<String> {
    let mut lines = input.lines().map(ToString::to_string).collect::<Vec<_>>();
    if input.ends_with('\n') {
        lines.push(String::new());
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn kv_line(label: &str, value: &str) -> StyledTextLine {
    StyledTextLine::styled(
        format!("{label:<9} {value}"),
        Style::default().fg(Color::White),
    )
}

struct TuiEventSink {
    runtime: Arc<Mutex<TuiRuntime>>,
}

impl EngineEventSink for TuiEventSink {
    fn on_event(&mut self, event: &zetta_protocol::EngineEvent) -> Result<()> {
        let mut runtime = self.runtime.lock().expect("tui runtime");
        runtime.push_activity_entry(activity_entry_from_event(event));
        if matches!(
            event,
            zetta_protocol::EngineEvent::AssistantMessagePersisted { .. }
        ) {
            runtime.state.pending_assistant.clear();
        }
        runtime.render()?;
        Ok(())
    }
}

struct TuiModelStreamSink {
    runtime: Arc<Mutex<TuiRuntime>>,
}

impl ModelStreamSink for TuiModelStreamSink {
    fn on_text_delta(&mut self, delta: &str) -> Result<()> {
        let mut runtime = self.runtime.lock().expect("tui runtime");
        runtime.state.pending_assistant.push_str(delta);
        runtime.render()?;
        Ok(())
    }

    fn on_message_end(&mut self) -> Result<()> {
        let mut runtime = self.runtime.lock().expect("tui runtime");
        runtime.render()?;
        Ok(())
    }
}

fn display_width(input: &str) -> usize {
    UnicodeWidthStr::width(input)
}

fn activity_entry_from_event(event: &zetta_protocol::EngineEvent) -> TuiActivityEntry {
    match event {
        zetta_protocol::EngineEvent::SessionLoaded { session_id, is_new } => TuiActivityEntry {
            badge: "turn".to_string(),
            title: format!(
                "session={} state={}",
                session_id,
                if *is_new { "new" } else { "resume" }
            ),
            detail: None,
            tone: ActivityTone::Neutral,
        },
        zetta_protocol::EngineEvent::UserMessagePersisted { .. } => TuiActivityEntry {
            badge: "turn".to_string(),
            title: "user message persisted".to_string(),
            detail: None,
            tone: ActivityTone::Neutral,
        },
        zetta_protocol::EngineEvent::ToolCallRequested { call } => TuiActivityEntry {
            badge: "tool".to_string(),
            title: format!("request {}", call.name),
            detail: Some(summarize_json_inline(&call.input, 72)),
            tone: ActivityTone::Running,
        },
        zetta_protocol::EngineEvent::ToolCallDenied { call, reason } => TuiActivityEntry {
            badge: "tool".to_string(),
            title: format!("denied {}", call.name),
            detail: Some(reason.clone()),
            tone: ActivityTone::Warning,
        },
        zetta_protocol::EngineEvent::ToolCallFailed { call, error } => TuiActivityEntry {
            badge: "tool".to_string(),
            title: format!("failed {}", call.name),
            detail: Some(error.clone()),
            tone: ActivityTone::Error,
        },
        zetta_protocol::EngineEvent::ToolCallCompleted { result } => TuiActivityEntry {
            badge: "tool".to_string(),
            title: format!("done {}", result.name),
            detail: Some(summarize_json_inline(&result.output, 72)),
            tone: ActivityTone::Success,
        },
        zetta_protocol::EngineEvent::AssistantMessagePersisted { message } => TuiActivityEntry {
            badge: "assistant".to_string(),
            title: summarize_history_content(&message.content),
            detail: None,
            tone: ActivityTone::Assistant,
        },
        zetta_protocol::EngineEvent::TurnFinished { session_id } => TuiActivityEntry {
            badge: "summary".to_string(),
            title: format!("session={session_id} finished"),
            detail: None,
            tone: ActivityTone::Success,
        },
    }
}

fn summarize_json_inline(value: &Value, max_len: usize) -> String {
    let serialized = value.to_string();
    let mut summary = serialized.chars().take(max_len).collect::<String>();
    if serialized.chars().count() > max_len {
        summary.push_str("...");
    }
    summary
}

#[derive(Default)]
struct SessionOverview {
    user_turns: usize,
    assistant_messages: usize,
    tool_messages: usize,
    completed_tools: usize,
    denied_tools: usize,
    failed_tools: usize,
    invalid_tool_calls: usize,
    tool_usage: BTreeMap<String, usize>,
}

fn print_session_overview(session: &zetta_protocol::SessionSnapshot) {
    let overview = build_session_overview(session);
    println!("session_id: {}", session.session_id);
    println!("updated_at: {}", session.updated_at.to_rfc3339());
    println!("messages: {}", session.messages.len());
    println!("user_turns: {}", overview.user_turns);
    println!("assistant_messages: {}", overview.assistant_messages);
    println!("tool_messages: {}", overview.tool_messages);
    println!("tool_completed: {}", overview.completed_tools);
    println!("tool_denied: {}", overview.denied_tools);
    println!("tool_failed: {}", overview.failed_tools);
    println!("tool_invalid: {}", overview.invalid_tool_calls);
    if !overview.tool_usage.is_empty() {
        println!(
            "tool_usage: {}",
            overview
                .tool_usage
                .iter()
                .map(|(name, count)| format!("{name}={count}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !session.tags.is_empty() {
        println!("tags: {}", session.tags.join(", "));
    }
    if !session.metadata.is_empty() {
        println!(
            "metadata: {}",
            session
                .metadata
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn build_session_overview(session: &zetta_protocol::SessionSnapshot) -> SessionOverview {
    let mut overview = SessionOverview::default();

    for message in &session.messages {
        match message.role {
            zetta_protocol::MessageRole::User => overview.user_turns += 1,
            zetta_protocol::MessageRole::Assistant => overview.assistant_messages += 1,
            zetta_protocol::MessageRole::Tool => {
                overview.tool_messages += 1;
                if let Some((tool_name, status)) = parse_tool_message_metadata(&message.content) {
                    *overview.tool_usage.entry(tool_name).or_insert(0) += 1;
                    match status.as_str() {
                        "completed" => overview.completed_tools += 1,
                        "denied" => overview.denied_tools += 1,
                        "failed" => overview.failed_tools += 1,
                        "invalid_call" => overview.invalid_tool_calls += 1,
                        _ => {}
                    }
                }
            }
            zetta_protocol::MessageRole::System => {}
        }
    }

    overview
}

fn parse_tool_message_metadata(content: &str) -> Option<(String, String)> {
    let value = serde_json::from_str::<Value>(content).ok()?;
    let object = value.as_object()?;
    if object.get("type")?.as_str()? != "tool_result" {
        return None;
    }
    Some((
        object.get("tool_name")?.as_str()?.to_string(),
        object
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string(),
    ))
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
Use tools when they materially help. Prefer native tool-calling when the model/runtime supports it. If native tool-calling is unavailable, respond with exactly one line in the form `/tool <name> <payload>` and no extra text.\n\
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
        guidance.push(
            "If the user explicitly asked for an edit, do not stop after inspection; make the edit once you have enough context."
                .to_string(),
        );
    }

    if tool_names.contains("glob") {
        guidance.push(
            "For repository structure, file lists, or extension-based discovery, prefer `glob` instead of `bash`."
                .to_string(),
        );
        guidance.push(
            "Keep `glob` patterns focused and set `max_results` when listing broad directories to avoid oversized outputs."
                .to_string(),
        );
    }

    if tool_names.contains("grep") {
        guidance.push(
            "For code or text search, prefer `grep` instead of shell `find`/`grep` pipelines."
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
            "Use `bash` for verification commands only when file tools are insufficient or when you need to run project commands."
                .to_string(),
        );
        guidance.push(
            "Keep `bash` calls to a single non-destructive command. Do not use shell pipelines, chaining, redirection, or `find ... | head` style constructs."
                .to_string(),
        );
        guidance.push(
            "If a `bash` call is denied, immediately rewrite the plan using `glob`, `grep`, `file_read`, or `file_read_lines` instead of retrying the same shell pattern."
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
    if tool_names.contains("glob") {
        examples.push(
            r#"- List Rust files: /tool glob {"pattern":"crates/**/*.rs","max_results":40}"#
                .to_string(),
        );
    }
    if tool_names.contains("grep") {
        examples.push(
            r#"- Search by content: /tool grep {"pattern":"MyFunction","max_results":20}"#
                .to_string(),
        );
    }
    if tool_names.contains("bash") {
        examples.push(
            r#"- Run one verification command: /tool bash {"command":"cargo test"}"#.to_string(),
        );
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

    use super::{
        build_session_overview, default_openai_system_prompt, display_width,
        latest_assistant_message, parse_repl_command, render_cli_error_lines, render_repl_prompt,
        resolve_openai_options, search_session_messages, split_text_lines,
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
