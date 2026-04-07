use super::*;

pub(crate) fn print_cli_error(error: &Error) {
    for line in render_cli_error_lines(error) {
        eprintln!("{line}");
    }
}

pub(crate) fn build_agent_engine(
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

pub(crate) async fn run_agent_turn(
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
