use super::*;

pub(crate) async fn run_repl(
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
