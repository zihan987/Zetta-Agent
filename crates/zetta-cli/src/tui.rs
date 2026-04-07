use super::*;

pub(crate) async fn run_tui(
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
            input_cursor: 0,
            input_history: tui_input_history_from_session(session.as_ref()),
            input_history_index: None,
            input_history_draft: None,
            pending_assistant: String::new(),
            activity_entries: vec![TuiActivityEntry {
                badge: "HELP".to_string(),
                title: "Tab focus • ? help • Enter submit • Shift+Enter newline".to_string(),
                detail: Some(
                    "Focus a pane, then use ↑/↓ to scroll • paste is supported • Alt+P/N recalls prompt history • /help shows slash commands"
                        .to_string(),
                ),
                tone: ActivityTone::Neutral,
            }],
            last_error: None,
            busy: false,
            busy_started_at: None,
            queued_prompts: VecDeque::new(),
            session,
            transcript_scroll: 0,
            activity_scroll: 0,
            transcript_unseen: 0,
            activity_unseen: 0,
        },
        terminal,
    )));

    runtime.lock().expect("tui runtime").render()?;

    loop {
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => match handle_tui_key(runtime.clone(), key)? {
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
                    TuiAction::LocalCommand(command) => {
                        execute_tui_local_command(
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
                            command,
                        )
                        .await?;
                    }
                },
                Event::Paste(pasted) => {
                    let mut runtime = runtime.lock().expect("tui runtime");
                    if runtime.state.focus == TuiFocus::Prompt && !runtime.state.busy {
                        runtime.insert_input_text(&pasted);
                        runtime.render()?;
                    }
                }
                _ => {}
            }
        } else {
            runtime
                .lock()
                .expect("tui runtime")
                .maybe_tick_busy_render()?;
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
    let next_prompt = {
        let mut runtime = runtime.lock().expect("tui runtime");
        let prompt = runtime.state.input.trim().to_string();
        runtime.record_input_history(&prompt);
        runtime.state.input.clear();
        runtime.state.input_cursor = 0;
        runtime.state.pending_assistant.clear();
        runtime.state.last_error = None;
        runtime.state.busy = true;
        runtime.state.busy_started_at = Some(Instant::now());
        runtime.clear_history_navigation();
        runtime.push_event_line(format!("[submit] {}", summarize_history_content(&prompt)));
        runtime.render()?;
        prompt
    };

    run_tui_prompt_loop(
        cli,
        store,
        config_store,
        hook_config_store,
        provider_config_store,
        cli_overrides,
        cli_hook_overrides,
        cwd,
        workspace_root,
        runtime,
        next_prompt,
    )
    .await
}

async fn run_tui_prompt_loop(
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
    mut next_prompt: String,
) -> Result<()> {
    loop {
        let (session_id, active_provider, permission_mode) = {
            let runtime = runtime.lock().expect("tui runtime");
            (
                runtime.state.session_id,
                runtime.state.active_provider.clone(),
                runtime.state.permission_mode,
            )
        };

        let effective_overrides = effective_permission_overrides(cli_overrides, permission_mode);
        let engine = build_agent_engine(
            cli,
            store.clone(),
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
            prompt: next_prompt.clone(),
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

        let mut runtime_guard = runtime.lock().expect("tui runtime");
        runtime_guard.state.pending_assistant.clear();

        match output {
            Ok(output) => {
                runtime_guard.state.session_id = output.session.session_id;
                runtime_guard.state.session = Some(output.session);
                for failure in output.hook_failures {
                    runtime_guard.push_activity_entry(TuiActivityEntry {
                        badge: "HOOK".to_string(),
                        title: format!("{} failed", failure.handler_name),
                        detail: Some(failure.error),
                        tone: ActivityTone::Warning,
                    });
                }
            }
            Err(error) => {
                let message = render_cli_error_lines(&error).join(" | ");
                runtime_guard.state.last_error = Some(message.clone());
                runtime_guard.push_activity_entry(TuiActivityEntry {
                    badge: "ERR".to_string(),
                    title: "model request failed".to_string(),
                    detail: Some(message),
                    tone: ActivityTone::Error,
                });
            }
        }

        if let Some(queued_prompt) = runtime_guard.state.queued_prompts.pop_front() {
            runtime_guard.state.busy = true;
            runtime_guard.state.busy_started_at = Some(Instant::now());
            runtime_guard.push_activity_entry(TuiActivityEntry {
                badge: "QUEUE".to_string(),
                title: "running queued follow-up".to_string(),
                detail: Some(summarize_history_content(&queued_prompt)),
                tone: ActivityTone::Running,
            });
            runtime_guard.render()?;
            next_prompt = queued_prompt;
            continue;
        }

        runtime_guard.state.busy = false;
        runtime_guard.state.busy_started_at = None;
        runtime_guard.render()?;
        break;
    }

    Ok(())
}

fn push_tui_activity(
    runtime: &Arc<Mutex<TuiRuntime>>,
    badge: &str,
    title: impl Into<String>,
    detail: Option<String>,
    tone: ActivityTone,
) -> Result<()> {
    let mut runtime = runtime.lock().expect("tui runtime");
    runtime.push_activity_entry(TuiActivityEntry {
        badge: badge.to_string(),
        title: title.into(),
        detail,
        tone,
    });
    runtime.render()?;
    Ok(())
}

enum TuiAction {
    None,
    Submit,
    LocalCommand(ReplCommand),
    Exit,
}

async fn execute_tui_local_command(
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
    command: ReplCommand,
) -> Result<()> {
    match command {
        ReplCommand::Help => {
            let mut runtime = runtime.lock().expect("tui runtime");
            runtime.state.help_overlay = true;
            runtime.render()?;
        }
        ReplCommand::Exit => {
            push_tui_activity(
                &runtime,
                "EXIT",
                "Use Esc or Ctrl+C to leave the TUI",
                Some("Slash commands stay inside the current fullscreen session.".to_string()),
                ActivityTone::Neutral,
            )?;
        }
        ReplCommand::Session => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            push_tui_activity(
                &runtime,
                "SESSION",
                format!("current session {session_id}"),
                None,
                ActivityTone::Neutral,
            )?;
        }
        ReplCommand::Tools => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            let tool_context = build_tool_context(
                cli_overrides,
                config_store,
                cwd,
                workspace_root,
                Some(session_id),
            )?;
            let registry = build_registry();
            let visible = registry.visible_names(&tool_context);
            push_tui_activity(
                &runtime,
                "TOOLS",
                format!("{} visible tools", visible.len()),
                Some(visible.join(", ")),
                ActivityTone::Neutral,
            )?;
        }
        ReplCommand::History => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            match store.load(&session_id).await? {
                Some(session) => {
                    push_tui_activity(
                        &runtime,
                        "HISTORY",
                        format!("{} messages", session.messages.len()),
                        Some(session_history_text(&session)),
                        ActivityTone::Neutral,
                    )?;
                }
                None => {
                    push_tui_activity(
                        &runtime,
                        "HISTORY",
                        "current session is empty",
                        None,
                        ActivityTone::Warning,
                    )?;
                }
            }
        }
        ReplCommand::Search(query) => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            match store.load(&session_id).await? {
                Some(session) => {
                    let matches = search_session_messages(&session, &query);
                    push_tui_activity(
                        &runtime,
                        "SEARCH",
                        format!("{} matches for `{query}`", matches.len()),
                        Some(session_search_text(&session, &query)),
                        if matches.is_empty() {
                            ActivityTone::Warning
                        } else {
                            ActivityTone::Neutral
                        },
                    )?;
                }
                None => {
                    push_tui_activity(
                        &runtime,
                        "SEARCH",
                        "current session is empty",
                        None,
                        ActivityTone::Warning,
                    )?;
                }
            }
        }
        ReplCommand::Last => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            match store.load(&session_id).await? {
                Some(session) => {
                    push_tui_activity(
                        &runtime,
                        "LAST",
                        "latest assistant reply",
                        Some(
                            latest_assistant_message(&session)
                                .unwrap_or("<no assistant message>")
                                .to_string(),
                        ),
                        ActivityTone::Assistant,
                    )?;
                }
                None => {
                    push_tui_activity(
                        &runtime,
                        "LAST",
                        "current session is empty",
                        None,
                        ActivityTone::Warning,
                    )?;
                }
            }
        }
        ReplCommand::Write(path) => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            match store.load(&session_id).await? {
                Some(session) => {
                    let Some(content) = latest_assistant_message(&session) else {
                        push_tui_activity(
                            &runtime,
                            "WRITE",
                            "no assistant reply to write",
                            None,
                            ActivityTone::Warning,
                        )?;
                        return Ok(());
                    };
                    let path_buf = PathBuf::from(path);
                    write_text_file(&path_buf, content)?;
                    push_tui_activity(
                        &runtime,
                        "WRITE",
                        format!("saved latest reply to {}", path_buf.display()),
                        None,
                        ActivityTone::Success,
                    )?;
                }
                None => {
                    push_tui_activity(
                        &runtime,
                        "WRITE",
                        "current session is empty",
                        None,
                        ActivityTone::Warning,
                    )?;
                }
            }
        }
        ReplCommand::Show => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            match store.load(&session_id).await? {
                Some(session) => {
                    push_tui_activity(
                        &runtime,
                        "SHOW",
                        format!("session {} summary", session.session_id),
                        Some(session_summary_text(&session)),
                        ActivityTone::Neutral,
                    )?;
                }
                None => {
                    push_tui_activity(
                        &runtime,
                        "SHOW",
                        "current session is empty",
                        None,
                        ActivityTone::Warning,
                    )?;
                }
            }
        }
        ReplCommand::New => {
            let mut runtime = runtime.lock().expect("tui runtime");
            runtime.state.session_id = SessionId::new();
            runtime.state.session = None;
            runtime.state.focus = TuiFocus::Prompt;
            runtime.state.input_history.clear();
            runtime.state.input_history_index = None;
            runtime.state.input_history_draft = None;
            runtime.state.input.clear();
            runtime.state.input_cursor = 0;
            runtime.state.pending_assistant.clear();
            runtime.state.queued_prompts.clear();
            runtime.state.last_error = None;
            runtime.reset_scrolls();
            let session_id = runtime.state.session_id;
            runtime.push_activity_entry(TuiActivityEntry {
                badge: "SESSION".to_string(),
                title: format!("switched to new session {session_id}"),
                detail: None,
                tone: ActivityTone::Success,
            });
            runtime.render()?;
        }
        ReplCommand::Reset => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            store.delete(&session_id).await?;
            let mut runtime = runtime.lock().expect("tui runtime");
            runtime.state.session = None;
            runtime.state.pending_assistant.clear();
            runtime.state.last_error = None;
            runtime.state.queued_prompts.clear();
            runtime.reset_scrolls();
            runtime.push_activity_entry(TuiActivityEntry {
                badge: "RESET".to_string(),
                title: "cleared current session history".to_string(),
                detail: Some(session_id.to_string()),
                tone: ActivityTone::Success,
            });
            runtime.render()?;
        }
        ReplCommand::Trim(turns) => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            let Some(mut session) = store.load(&session_id).await? else {
                push_tui_activity(
                    &runtime,
                    "TRIM",
                    "current session is empty",
                    None,
                    ActivityTone::Warning,
                )?;
                return Ok(());
            };
            let original_len = session.messages.len();
            let trimmed = trim_session_to_last_user_turns(&mut session, turns);
            if trimmed == 0 {
                push_tui_activity(
                    &runtime,
                    "TRIM",
                    "no messages trimmed",
                    None,
                    ActivityTone::Neutral,
                )?;
                return Ok(());
            }
            session.updated_at = Utc::now();
            store.save(&session).await?;
            let mut runtime = runtime.lock().expect("tui runtime");
            runtime.state.session = Some(session);
            runtime.reset_scrolls();
            runtime.push_activity_entry(TuiActivityEntry {
                badge: "TRIM".to_string(),
                title: format!("kept last {turns} user turns"),
                detail: Some(format!(
                    "trimmed {trimmed} messages; kept {} messages",
                    original_len - trimmed
                )),
                tone: ActivityTone::Success,
            });
            runtime.render()?;
        }
        ReplCommand::Retry | ReplCommand::Rerun(_) => {
            let turns_back = match command {
                ReplCommand::Retry => 1,
                ReplCommand::Rerun(turns_back) => turns_back,
                _ => unreachable!(),
            };
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            let Some(mut session) = store.load(&session_id).await? else {
                push_tui_activity(
                    &runtime,
                    "RERUN",
                    "current session is empty",
                    None,
                    ActivityTone::Warning,
                )?;
                return Ok(());
            };
            let Some((rerun_index, rerun_prompt)) = user_turn_from_end(&session, turns_back) else {
                push_tui_activity(
                    &runtime,
                    "RERUN",
                    format!("user turn {turns_back} from the end was not found"),
                    None,
                    ActivityTone::Warning,
                )?;
                return Ok(());
            };
            session.messages.truncate(rerun_index);
            session.updated_at = Utc::now();
            store.save(&session).await?;
            {
                let mut runtime = runtime.lock().expect("tui runtime");
                runtime.state.session = Some(session);
                runtime.state.pending_assistant.clear();
                runtime.state.last_error = None;
                runtime.state.busy = true;
                runtime.state.busy_started_at = Some(Instant::now());
                runtime.push_activity_entry(TuiActivityEntry {
                    badge: "RERUN".to_string(),
                    title: if turns_back == 1 {
                        "retrying latest user turn".to_string()
                    } else {
                        format!("rerunning user turn {turns_back} from the end")
                    },
                    detail: Some(summarize_history_content(&rerun_prompt)),
                    tone: ActivityTone::Running,
                });
                runtime.render()?;
            }
            run_tui_prompt_loop(
                cli,
                store,
                config_store,
                hook_config_store,
                provider_config_store,
                cli_overrides,
                cli_hook_overrides,
                cwd,
                workspace_root,
                runtime,
                rerun_prompt,
            )
            .await?;
        }
        ReplCommand::Export(path) => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            match store.load(&session_id).await? {
                Some(session) => {
                    let path_buf = PathBuf::from(path);
                    write_json_file(&path_buf, &session)?;
                    push_tui_activity(
                        &runtime,
                        "EXPORT",
                        format!("exported session to {}", path_buf.display()),
                        None,
                        ActivityTone::Success,
                    )?;
                }
                None => {
                    push_tui_activity(
                        &runtime,
                        "EXPORT",
                        "current session is empty",
                        None,
                        ActivityTone::Warning,
                    )?;
                }
            }
        }
        ReplCommand::Provider => {
            let active_provider = runtime
                .lock()
                .expect("tui runtime")
                .state
                .active_provider
                .clone();
            let lines = provider_summary_lines(active_provider.as_deref(), provider_config_store)?;
            push_tui_activity(
                &runtime,
                "PROVIDER",
                "provider summary",
                Some(lines.join("\n")),
                ActivityTone::Neutral,
            )?;
        }
        ReplCommand::Config => {
            let (session_id, active_provider, permission_mode) = {
                let runtime = runtime.lock().expect("tui runtime");
                (
                    runtime.state.session_id,
                    runtime.state.active_provider.clone(),
                    runtime.state.permission_mode,
                )
            };
            let lines = runtime_summary_lines(
                cli,
                config_store,
                cli_overrides,
                cwd,
                workspace_root,
                session_id,
                active_provider.as_deref(),
                permission_mode,
                CliUiMode::Pretty,
            )?;
            push_tui_activity(
                &runtime,
                "CONFIG",
                "runtime summary",
                Some(lines.join("\n")),
                ActivityTone::Neutral,
            )?;
        }
        ReplCommand::Overview => {
            let session = runtime.lock().expect("tui runtime").state.session.clone();
            match session {
                Some(session) => {
                    push_tui_activity(
                        &runtime,
                        "OVERVIEW",
                        "session overview",
                        Some(session_overview_text(&session)),
                        ActivityTone::Neutral,
                    )?;
                }
                None => {
                    push_tui_activity(
                        &runtime,
                        "OVERVIEW",
                        "current session is empty",
                        None,
                        ActivityTone::Warning,
                    )?;
                }
            }
        }
        ReplCommand::Load(target_session_id) => match store.load(&target_session_id).await? {
            Some(session) => {
                let mut runtime = runtime.lock().expect("tui runtime");
                runtime.state.session_id = target_session_id;
                runtime.state.session = Some(session.clone());
                runtime.state.input_history = tui_input_history_from_session(Some(&session));
                runtime.state.input_history_index = None;
                runtime.state.input_history_draft = None;
                runtime.state.pending_assistant.clear();
                runtime.state.queued_prompts.clear();
                runtime.reset_scrolls();
                runtime.push_activity_entry(TuiActivityEntry {
                    badge: "LOAD".to_string(),
                    title: format!("loaded session {target_session_id}"),
                    detail: None,
                    tone: ActivityTone::Success,
                });
                runtime.render()?;
            }
            None => {
                push_tui_activity(
                    &runtime,
                    "LOAD",
                    format!("session `{target_session_id}` not found"),
                    None,
                    ActivityTone::Warning,
                )?;
            }
        },
        ReplCommand::Fork => {
            let session_id = runtime.lock().expect("tui runtime").state.session_id;
            match store.load(&session_id).await? {
                Some(mut session) => {
                    let source_session_id = session_id;
                    let forked_session_id = SessionId::new();
                    let now = Utc::now();
                    session.session_id = forked_session_id;
                    session.created_at = now;
                    session.updated_at = now;
                    store.save(&session).await?;
                    let mut runtime = runtime.lock().expect("tui runtime");
                    runtime.state.session_id = forked_session_id;
                    runtime.state.session = Some(session.clone());
                    runtime.state.input_history = tui_input_history_from_session(Some(&session));
                    runtime.state.input_history_index = None;
                    runtime.state.input_history_draft = None;
                    runtime.reset_scrolls();
                    runtime.push_activity_entry(TuiActivityEntry {
                        badge: "FORK".to_string(),
                        title: format!("forked {source_session_id} -> {forked_session_id}"),
                        detail: None,
                        tone: ActivityTone::Success,
                    });
                    runtime.render()?;
                }
                None => {
                    let source_session_id = session_id;
                    let forked_session_id = SessionId::new();
                    let mut runtime = runtime.lock().expect("tui runtime");
                    runtime.state.session_id = forked_session_id;
                    runtime.state.session = None;
                    runtime.state.input_history.clear();
                    runtime.reset_scrolls();
                    runtime.push_activity_entry(TuiActivityEntry {
                        badge: "FORK".to_string(),
                        title: format!(
                            "forked empty session {source_session_id} -> {forked_session_id}"
                        ),
                        detail: None,
                        tone: ActivityTone::Success,
                    });
                    runtime.render()?;
                }
            }
        }
        ReplCommand::ProviderUse(provider_name) => {
            if resolve_provider_profile_by_name(Some(&provider_name), provider_config_store)?
                .is_some()
            {
                let mut runtime = runtime.lock().expect("tui runtime");
                runtime.state.active_provider = Some(provider_name.clone());
                runtime.push_activity_entry(TuiActivityEntry {
                    badge: "PROVIDER".to_string(),
                    title: format!("provider set to {provider_name}"),
                    detail: None,
                    tone: ActivityTone::Success,
                });
                runtime.render()?;
            } else {
                push_tui_activity(
                    &runtime,
                    "PROVIDER",
                    format!("provider `{provider_name}` not found"),
                    None,
                    ActivityTone::Warning,
                )?;
            }
        }
        ReplCommand::ProviderClear => {
            let mut runtime = runtime.lock().expect("tui runtime");
            runtime.state.active_provider = None;
            runtime.push_activity_entry(TuiActivityEntry {
                badge: "PROVIDER".to_string(),
                title: "cleared active provider".to_string(),
                detail: None,
                tone: ActivityTone::Success,
            });
            runtime.render()?;
        }
        ReplCommand::ModeShow => {
            let mode = runtime
                .lock()
                .expect("tui runtime")
                .state
                .permission_mode
                .unwrap_or(CliPermissionMode::WorkspaceWrite);
            push_tui_activity(
                &runtime,
                "MODE",
                format!("permission mode {}", mode.as_str()),
                None,
                ActivityTone::Neutral,
            )?;
        }
        ReplCommand::ModeSet(mode) => {
            let mut runtime = runtime.lock().expect("tui runtime");
            runtime.state.permission_mode = Some(mode);
            runtime.push_activity_entry(TuiActivityEntry {
                badge: "MODE".to_string(),
                title: format!("permission mode set to {}", mode.as_str()),
                detail: None,
                tone: ActivityTone::Success,
            });
            runtime.render()?;
        }
        ReplCommand::UiShow => {
            push_tui_activity(
                &runtime,
                "UI",
                "fullscreen terminal UI is active",
                Some(
                    "Use /help for keyboard shortcuts or switch back to REPL for text-mode UI toggles."
                        .to_string(),
                ),
                ActivityTone::Neutral,
            )?;
        }
        ReplCommand::UiSet(_)
        | ReplCommand::EventsShow
        | ReplCommand::EventsSet(_)
        | ReplCommand::JsonShow
        | ReplCommand::JsonSet(_) => {
            push_tui_activity(
                &runtime,
                "UI",
                "this slash command is only available in CLI/REPL mode",
                Some(
                    "The fullscreen TUI always renders locally; use /help and the Activity pane instead."
                        .to_string(),
                ),
                ActivityTone::Warning,
            )?;
        }
    }

    Ok(())
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
                if runtime.state.focus == TuiFocus::Prompt && !runtime.state.busy {
                    runtime.insert_input_text("\n");
                    runtime.render()?;
                    return Ok(TuiAction::None);
                }
            }
            KeyCode::Char('l') => {
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            KeyCode::Char('u') => {
                runtime.state.input.clear();
                runtime.state.input_cursor = 0;
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
                runtime.state.input.clear();
                runtime.state.input_cursor = 0;
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
        KeyCode::Char('k')
            if key.modifiers.is_empty() && runtime.state.focus != TuiFocus::Prompt =>
        {
            runtime.scroll_current_focus(3);
        }
        KeyCode::Char('j')
            if key.modifiers.is_empty() && runtime.state.focus != TuiFocus::Prompt =>
        {
            runtime.scroll_current_focus(-3);
        }
        KeyCode::Char('g')
            if key.modifiers.is_empty() && runtime.state.focus != TuiFocus::Prompt =>
        {
            runtime.jump_scroll_to_top();
        }
        KeyCode::Char('G')
            if key.modifiers.contains(KeyModifiers::SHIFT)
                && runtime.state.focus != TuiFocus::Prompt =>
        {
            runtime.jump_scroll_to_bottom();
        }
        KeyCode::F(1) => {
            runtime.state.help_overlay = true;
        }
        KeyCode::Backspace => {
            if runtime.state.focus == TuiFocus::Prompt {
                runtime.backspace_input();
            }
        }
        KeyCode::Delete => {
            if runtime.state.focus == TuiFocus::Prompt {
                runtime.delete_input();
            }
        }
        KeyCode::Left => {
            if runtime.state.focus == TuiFocus::Prompt {
                runtime.move_input_cursor_left();
            }
        }
        KeyCode::Right => {
            if runtime.state.focus == TuiFocus::Prompt {
                runtime.move_input_cursor_right();
            }
        }
        KeyCode::Home => {
            if runtime.state.focus == TuiFocus::Prompt {
                runtime.move_input_cursor_line_start();
            }
        }
        KeyCode::End => {
            if runtime.state.focus == TuiFocus::Prompt {
                runtime.move_input_cursor_line_end();
            }
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if runtime.state.focus == TuiFocus::Prompt {
                runtime.insert_input_text("\n");
            }
        }
        KeyCode::Enter => {
            let should_submit =
                runtime.state.focus == TuiFocus::Prompt && !runtime.state.input.trim().is_empty();
            let slash_command = if should_submit {
                parse_tui_slash_command(runtime.state.input.trim())
            } else {
                None
            };
            if let Some(parsed) = slash_command {
                runtime.state.input.clear();
                runtime.state.input_cursor = 0;
                runtime.clear_history_navigation();
                match parsed {
                    Ok(command) => {
                        runtime.render()?;
                        return Ok(TuiAction::LocalCommand(command));
                    }
                    Err(error) => {
                        runtime.state.last_error = Some(error.clone());
                        runtime.push_activity_entry(TuiActivityEntry {
                            badge: "CMD".to_string(),
                            title: "invalid slash command".to_string(),
                            detail: Some(error),
                            tone: ActivityTone::Warning,
                        });
                        runtime.render()?;
                        return Ok(TuiAction::None);
                    }
                }
            }
            if should_submit && runtime.state.busy {
                let prompt = runtime.state.input.trim().to_string();
                runtime.record_input_history(&prompt);
                runtime.state.queued_prompts.push_back(prompt.clone());
                runtime.state.input.clear();
                runtime.state.input_cursor = 0;
                runtime.push_activity_entry(TuiActivityEntry {
                    badge: "QUEUE".to_string(),
                    title: "queued follow-up".to_string(),
                    detail: Some(summarize_history_content(&prompt)),
                    tone: ActivityTone::Neutral,
                });
                runtime.render()?;
                return Ok(TuiAction::None);
            }
            runtime.render()?;
            return Ok(if should_submit {
                TuiAction::Submit
            } else {
                TuiAction::None
            });
        }
        KeyCode::Tab => {
            if runtime.state.focus == TuiFocus::Prompt {
                runtime.insert_input_text("    ");
            }
        }
        KeyCode::Char(character) => {
            if runtime.state.focus == TuiFocus::Prompt
                && !key.modifiers.contains(KeyModifiers::CONTROL)
            {
                let mut buffer = [0u8; 4];
                runtime.insert_input_text(character.encode_utf8(&mut buffer));
            }
        }
        _ => {}
    }

    runtime.render()?;
    Ok(TuiAction::None)
}

pub(crate) fn render_cli_error_lines(error: &Error) -> Vec<String> {
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

pub(crate) fn parse_repl_command(input: &str) -> Option<Result<ReplCommand, String>> {
    parse_local_command(input, ':')
}

pub(crate) fn parse_tui_slash_command(input: &str) -> Option<Result<ReplCommand, String>> {
    parse_local_command(input, '/')
}

fn parse_local_command(input: &str, prefix: char) -> Option<Result<ReplCommand, String>> {
    let trimmed = input.trim();
    let mut parts = trimmed.split_whitespace();
    let command = parts.next()?;
    let p = prefix.to_string();

    let parsed = match command {
        cmd if cmd == format!("{p}help") => Ok(ReplCommand::Help),
        cmd if cmd == format!("{p}exit") || cmd == format!("{p}quit") => Ok(ReplCommand::Exit),
        cmd if cmd == format!("{p}session") => Ok(ReplCommand::Session),
        cmd if cmd == format!("{p}tools") => Ok(ReplCommand::Tools),
        cmd if cmd == format!("{p}history") => Ok(ReplCommand::History),
        cmd if cmd == format!("{p}search") => {
            let query = trimmed[format!("{p}search").len()..].trim();
            if query.is_empty() {
                return Some(Err(format!("expected `{p}search <text>`")));
            }
            Ok(ReplCommand::Search(query.to_string()))
        }
        cmd if cmd == format!("{p}last") => Ok(ReplCommand::Last),
        cmd if cmd == format!("{p}write") => {
            let path = trimmed[format!("{p}write").len()..].trim();
            if path.is_empty() {
                return Some(Err(format!("expected `{p}write <path>`")));
            }
            Ok(ReplCommand::Write(path.to_string()))
        }
        cmd if cmd == format!("{p}show") => Ok(ReplCommand::Show),
        cmd if cmd == format!("{p}new") => Ok(ReplCommand::New),
        cmd if cmd == format!("{p}reset") => Ok(ReplCommand::Reset),
        cmd if cmd == format!("{p}trim") => {
            let value = trimmed[format!("{p}trim").len()..].trim();
            if value.is_empty() {
                return Some(Err(format!("expected `{p}trim <turns>`")));
            }
            match value.parse::<usize>() {
                Ok(turns) if turns > 0 => Ok(ReplCommand::Trim(turns)),
                Ok(_) => Err("invalid trim value `0`; expected a positive integer".to_string()),
                Err(error) => Err(format!("invalid trim value `{value}`: {error}")),
            }
        }
        cmd if cmd == format!("{p}retry") => Ok(ReplCommand::Retry),
        cmd if cmd == format!("{p}rerun") => {
            let value = trimmed[format!("{p}rerun").len()..].trim();
            if value.is_empty() {
                return Some(Err(format!("expected `{p}rerun <turns_back>`")));
            }
            match value.parse::<usize>() {
                Ok(turns_back) if turns_back > 0 => Ok(ReplCommand::Rerun(turns_back)),
                Ok(_) => Err("invalid rerun value `0`; expected a positive integer".to_string()),
                Err(error) => Err(format!("invalid rerun value `{value}`: {error}")),
            }
        }
        cmd if cmd == format!("{p}export") => {
            let path = trimmed[format!("{p}export").len()..].trim();
            if path.is_empty() {
                return Some(Err(format!("expected `{p}export <path>`")));
            }
            Ok(ReplCommand::Export(path.to_string()))
        }
        cmd if cmd == format!("{p}provider") => match parts.next() {
            None => Ok(ReplCommand::Provider),
            Some("use") => {
                let Some(provider_name) = parts.next() else {
                    return Some(Err(format!("expected `{p}provider use <name>`")));
                };
                if parts.next().is_some() {
                    return Some(Err(format!(
                        "expected only one provider name after `{p}provider use`"
                    )));
                }
                Ok(ReplCommand::ProviderUse(provider_name.to_string()))
            }
            Some("clear") => {
                if parts.next().is_some() {
                    return Some(Err(format!(
                        "expected `{p}provider clear` with no extra arguments"
                    )));
                }
                Ok(ReplCommand::ProviderClear)
            }
            Some(other) => Err(format!(
                "unknown provider subcommand `{other}`; use `{p}provider`, `{p}provider use <name>`, or `{p}provider clear`"
            )),
        },
        cmd if cmd == format!("{p}config") => Ok(ReplCommand::Config),
        cmd if cmd == format!("{p}overview") => Ok(ReplCommand::Overview),
        cmd if cmd == format!("{p}fork") => Ok(ReplCommand::Fork),
        cmd if cmd == format!("{p}mode") => match parts.next() {
            None => Ok(ReplCommand::ModeShow),
            Some(mode) => {
                if parts.next().is_some() {
                    return Some(Err(format!(
                        "expected `{p}mode <read-only|workspace-write|bypass-permissions>`"
                    )));
                }
                parse_repl_permission_mode(mode).map(ReplCommand::ModeSet)
            }
        },
        cmd if cmd == format!("{p}ui") => match parts.next() {
            None => Ok(ReplCommand::UiShow),
            Some(mode) => {
                if parts.next().is_some() {
                    return Some(Err(format!("expected `{p}ui <off|pretty|json>`")));
                }
                parse_repl_ui_mode(mode).map(ReplCommand::UiSet)
            }
        },
        cmd if cmd == format!("{p}events") => match parts.next() {
            None => Ok(ReplCommand::EventsShow),
            Some(value) => {
                if parts.next().is_some() {
                    return Some(Err(format!("expected `{p}events on` or `{p}events off`")));
                }
                parse_repl_toggle(value).map(ReplCommand::EventsSet)
            }
        },
        cmd if cmd == format!("{p}json") => match parts.next() {
            None => Ok(ReplCommand::JsonShow),
            Some(value) => {
                if parts.next().is_some() {
                    return Some(Err(format!("expected `{p}json on` or `{p}json off`")));
                }
                parse_repl_toggle(value).map(ReplCommand::JsonSet)
            }
        },
        cmd if cmd == format!("{p}load") => {
            let Some(session_id) = parts.next() else {
                return Some(Err(format!("expected `{p}load <session_id>`")));
            };
            if parts.next().is_some() {
                return Some(Err(format!("expected only one session id after `{p}load`")));
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

pub(crate) fn render_repl_prompt(
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

fn session_summary_text(session: &zetta_protocol::SessionSnapshot) -> String {
    let mut lines = vec![
        format!("session_id: {}", session.session_id),
        format!("messages: {}", session.messages.len()),
    ];
    if !session.tags.is_empty() {
        lines.push(format!("tags: {}", session.tags.join(", ")));
    }
    if !session.metadata.is_empty() {
        lines.push(format!(
            "metadata: {}",
            session
                .metadata
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if session.messages.is_empty() {
        lines.push("<empty session>".to_string());
        return lines.join("\n");
    }
    lines.extend(session.messages.iter().map(|message| {
        let role = match message.role {
            zetta_protocol::MessageRole::System => "system",
            zetta_protocol::MessageRole::User => "user",
            zetta_protocol::MessageRole::Assistant => "assistant",
            zetta_protocol::MessageRole::Tool => "tool",
        };
        format!("[{role}] {}", message.content)
    }));
    lines.join("\n")
}

pub(crate) fn print_session_summary(session: &zetta_protocol::SessionSnapshot) {
    println!("{}", session_summary_text(session));
}

pub(crate) fn latest_assistant_message(session: &zetta_protocol::SessionSnapshot) -> Option<&str> {
    session
        .messages
        .iter()
        .rev()
        .find(|message| matches!(message.role, zetta_protocol::MessageRole::Assistant))
        .map(|message| message.content.as_str())
}

pub(crate) fn print_session_history(session: &zetta_protocol::SessionSnapshot) {
    println!("{}", session_history_text(session));
}

pub(crate) fn print_session_search_results(session: &zetta_protocol::SessionSnapshot, query: &str) {
    println!("{}", session_search_text(session, query));
}

fn session_history_text(session: &zetta_protocol::SessionSnapshot) -> String {
    let mut lines = vec![
        format!("session_id: {}", session.session_id),
        format!("messages: {}", session.messages.len()),
    ];
    if session.messages.is_empty() {
        lines.push("<empty session>".to_string());
        return lines.join("\n");
    }

    lines.extend(session.messages.iter().enumerate().map(|(index, message)| {
        let role = match message.role {
            zetta_protocol::MessageRole::System => "system",
            zetta_protocol::MessageRole::User => "user",
            zetta_protocol::MessageRole::Assistant => "assistant",
            zetta_protocol::MessageRole::Tool => "tool",
        };
        format!(
            "{:>3}. [{}] {}",
            index + 1,
            role,
            summarize_history_content(&message.content)
        )
    }));
    lines.join("\n")
}

fn session_search_text(session: &zetta_protocol::SessionSnapshot, query: &str) -> String {
    let matches = search_session_messages(session, query);
    let mut lines = vec![
        format!("session_id: {}", session.session_id),
        format!("search: {query}"),
    ];
    if matches.is_empty() {
        lines.push("<no matches>".to_string());
        return lines.join("\n");
    }
    lines.extend(
        matches
            .into_iter()
            .map(|(index, role, content)| format!("{:>3}. [{}] {}", index + 1, role, content)),
    );
    lines.join("\n")
}

pub(crate) fn search_session_messages(
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

pub(crate) fn summarize_history_content(content: &str) -> String {
    const MAX_LEN: usize = 100;

    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut summary = normalized.chars().take(MAX_LEN).collect::<String>();
    if normalized.chars().count() > MAX_LEN {
        summary.push_str("...");
    }
    summary
}

pub(crate) fn user_turn_from_end(
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

pub(crate) fn trim_session_to_last_user_turns(
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

pub(crate) fn print_provider_summary(
    active_provider: Option<&str>,
    provider_config_store: &ProviderConfigStore,
) -> Result<()> {
    for line in provider_summary_lines(active_provider, provider_config_store)? {
        println!("{line}");
    }
    Ok(())
}

fn provider_summary_lines(
    active_provider: Option<&str>,
    provider_config_store: &ProviderConfigStore,
) -> Result<Vec<String>> {
    Ok(
        match resolve_provider_profile_by_name(active_provider, provider_config_store)? {
            Some(profile) => {
                let provider_name = active_provider.unwrap_or("<unnamed>");
                vec![
                    format!("provider: {provider_name}"),
                    serde_json::to_string_pretty(&profile)?,
                ]
            }
            None => vec!["provider: <none>".to_string()],
        },
    )
}

pub(crate) fn print_runtime_summary(
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
    for line in runtime_summary_lines(
        cli,
        config_store,
        cli_overrides,
        cwd,
        workspace_root,
        session_id,
        active_provider,
        permission_mode_override,
        ui_mode,
    )? {
        println!("{line}");
    }
    Ok(())
}

fn runtime_summary_lines(
    cli: &Cli,
    config_store: &PermissionConfigStore,
    cli_overrides: &PersistentPermissionConfig,
    cwd: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: SessionId,
    active_provider: Option<&str>,
    permission_mode_override: Option<CliPermissionMode>,
    ui_mode: CliUiMode,
) -> Result<Vec<String>> {
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

    Ok(vec![
        format!("session_id: {session_id}"),
        format!("cwd: {}", cwd.display()),
        format!("workspace_root: {}", policy.workspace_root().display()),
        format!("permission_mode: {:?}", policy.mode()),
        format!("config_dir: {}", cli.config_dir.display()),
        format!("session_dir: {}", cli.session_dir.display()),
        format!("stream_output: {}", cli.stream_output),
        format!("ui_mode: {}", ui_mode.as_str()),
        format!(
            "events: {}",
            if ui_mode == CliUiMode::Pretty {
                "on"
            } else {
                "off"
            }
        ),
        format!(
            "json: {}",
            if ui_mode == CliUiMode::Json {
                "on"
            } else {
                "off"
            }
        ),
        format!("provider: {}", active_provider.unwrap_or("<none>")),
        format!(
            "model_driver: {}",
            if provider_profile.is_some() {
                "openai-compatible"
            } else {
                match cli.model_driver {
                    CliModelDriver::RuleBased => "rule-based",
                    CliModelDriver::OpenaiCompatible => "openai-compatible",
                }
            }
        ),
        format!("api_key_env: {}", resolved.api_key_env),
        format!(
            "model_name: {}",
            resolved.model_name.as_deref().unwrap_or("<unset>")
        ),
        format!(
            "api_base: {}",
            resolved.api_base.as_deref().unwrap_or("<default>")
        ),
        format!(
            "readable_roots: {}",
            policy
                .readable_roots()
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        format!(
            "writable_roots: {}",
            policy
                .writable_roots()
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        format!("visible_tools: {}", visible_tools.join(", ")),
    ])
}

pub(crate) struct StderrTurnPresenter {
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
    pub(crate) fn new(
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
    input_cursor: usize,
    input_history: Vec<String>,
    input_history_index: Option<usize>,
    input_history_draft: Option<String>,
    pending_assistant: String,
    activity_entries: Vec<TuiActivityEntry>,
    last_error: Option<String>,
    busy: bool,
    busy_started_at: Option<Instant>,
    queued_prompts: VecDeque<String>,
    session: Option<zetta_protocol::SessionSnapshot>,
    transcript_scroll: usize,
    activity_scroll: usize,
    transcript_unseen: usize,
    activity_unseen: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TuiFocus {
    Conversation,
    Activity,
    Prompt,
}

#[derive(Clone, Debug)]
pub(crate) struct StyledTextLine {
    pub(crate) text: String,
    pub(crate) style: Style,
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
    last_busy_tick_second: Option<u64>,
}

impl TuiRuntime {
    fn new(state: TuiState, terminal: TuiTerminal) -> Self {
        Self {
            state,
            terminal,
            last_busy_tick_second: None,
        }
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
        if self.state.activity_scroll > 0 {
            self.state.activity_unseen = self.state.activity_unseen.saturating_add(1);
        }
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

    fn current_busy_elapsed(&self) -> Option<Duration> {
        self.state
            .busy_started_at
            .map(|started_at| started_at.elapsed())
    }

    fn current_busy_elapsed_seconds(&self) -> Option<u64> {
        self.current_busy_elapsed().map(|elapsed| elapsed.as_secs())
    }

    fn maybe_tick_busy_render(&mut self) -> Result<()> {
        if !self.state.busy {
            self.last_busy_tick_second = None;
            return Ok(());
        }

        let Some(seconds) = self.current_busy_elapsed_seconds() else {
            return Ok(());
        };

        if self.last_busy_tick_second == Some(seconds) {
            return Ok(());
        }

        self.last_busy_tick_second = Some(seconds);
        self.render()
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
        self.state.input_cursor = self.state.input.len();
    }

    fn history_next(&mut self) {
        let Some(index) = self.state.input_history_index else {
            return;
        };

        if index + 1 < self.state.input_history.len() {
            let next_index = index + 1;
            self.state.input_history_index = Some(next_index);
            self.state.input = self.state.input_history[next_index].clone();
            self.state.input_cursor = self.state.input.len();
        } else {
            self.state.input_history_index = None;
            self.state.input = self.state.input_history_draft.take().unwrap_or_default();
            self.state.input_cursor = self.state.input.len();
        }
    }

    fn clear_history_navigation(&mut self) {
        self.state.input_history_index = None;
        self.state.input_history_draft = None;
    }

    fn insert_input_text(&mut self, text: &str) {
        let cursor = clamp_cursor_boundary(&self.state.input, self.state.input_cursor);
        self.state.input.insert_str(cursor, text);
        self.state.input_cursor = cursor + text.len();
        self.clear_history_navigation();
    }

    fn backspace_input(&mut self) {
        let cursor = clamp_cursor_boundary(&self.state.input, self.state.input_cursor);
        let Some(previous) = previous_char_boundary(&self.state.input, cursor) else {
            return;
        };
        self.state.input.drain(previous..cursor);
        self.state.input_cursor = previous;
        self.clear_history_navigation();
    }

    fn delete_input(&mut self) {
        let cursor = clamp_cursor_boundary(&self.state.input, self.state.input_cursor);
        let Some(next) = next_char_boundary(&self.state.input, cursor) else {
            return;
        };
        self.state.input.drain(cursor..next);
        self.state.input_cursor = cursor;
        self.clear_history_navigation();
    }

    fn move_input_cursor_left(&mut self) {
        let cursor = clamp_cursor_boundary(&self.state.input, self.state.input_cursor);
        if let Some(previous) = previous_char_boundary(&self.state.input, cursor) {
            self.state.input_cursor = previous;
        }
    }

    fn move_input_cursor_right(&mut self) {
        let cursor = clamp_cursor_boundary(&self.state.input, self.state.input_cursor);
        if let Some(next) = next_char_boundary(&self.state.input, cursor) {
            self.state.input_cursor = next;
        }
    }

    fn move_input_cursor_line_start(&mut self) {
        let cursor = clamp_cursor_boundary(&self.state.input, self.state.input_cursor);
        self.state.input_cursor = line_start_boundary(&self.state.input, cursor);
    }

    fn move_input_cursor_line_end(&mut self) {
        let cursor = clamp_cursor_boundary(&self.state.input, self.state.input_cursor);
        self.state.input_cursor = line_end_boundary(&self.state.input, cursor);
    }

    fn jump_scroll_to_top(&mut self) {
        match self.state.focus {
            TuiFocus::Conversation => self.state.transcript_scroll = usize::MAX / 4,
            TuiFocus::Activity => self.state.activity_scroll = usize::MAX / 4,
            TuiFocus::Prompt => {}
        }
    }

    fn jump_scroll_to_bottom(&mut self) {
        match self.state.focus {
            TuiFocus::Conversation => {
                self.state.transcript_scroll = 0;
                self.state.transcript_unseen = 0;
            }
            TuiFocus::Activity => {
                self.state.activity_scroll = 0;
                self.state.activity_unseen = 0;
            }
            TuiFocus::Prompt => {}
        }
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
        if self.state.transcript_scroll == 0 {
            self.state.transcript_unseen = 0;
        }
    }

    fn scroll_activity(&mut self, delta: isize) {
        self.state.activity_scroll = apply_scroll_delta(self.state.activity_scroll, delta);
        if self.state.activity_scroll == 0 {
            self.state.activity_unseen = 0;
        }
    }

    fn reset_scrolls(&mut self) {
        self.state.transcript_scroll = 0;
        self.state.activity_scroll = 0;
        self.state.transcript_unseen = 0;
        self.state.activity_unseen = 0;
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

pub(crate) fn tui_input_history_from_session(
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

pub(crate) fn pane_title(base: &str, scroll: usize, unseen: usize) -> String {
    let mut title = format!(" {base}");
    if scroll > 0 {
        title.push_str(" • paused");
    }
    if unseen > 0 {
        title.push_str(&format!(" • {unseen} new"));
    }
    title.push(' ');
    title
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
        pane_title(
            "Conversation",
            state.transcript_scroll,
            state.transcript_unseen,
        ),
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
        pane_title("Activity", state.activity_scroll, state.activity_unseen),
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
            "Enter              submit, or queue a follow-up while a turn is already running",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "Shift+Enter        insert a newline in the Prompt composer",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "Paste              multi-line paste goes directly into the Prompt pane",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "Alt+P / Alt+N      cycle through prior submitted prompts in Prompt focus",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "/help /new /session /tools /provider /mode /overview",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "← / → / Home / End edit within the Prompt composer",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "Ctrl+N / Ctrl+U    new session / clear prompt",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "j / k / g / G      vim-style scroll for focused Conversation or Activity",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "? / F1 / Esc       open, close, or dismiss this help overlay",
            Style::default().fg(Color::Gray),
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

    let status_height = if state.busy { 1 } else { 0 };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(status_height),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let status_area = sections[0];
    let input_area = sections[1];
    let hint_area = sections[2];
    let inner_width = input_area.width as usize;
    let inner_height = input_area.height as usize;
    let (lines, cursor_line, cursor_col) =
        composer_display_lines(state, inner_width.max(1), inner_height.max(1));

    if state.busy {
        frame.render_widget(
            Paragraph::new(tui_busy_status_line(state)).style(Style::default().fg(Color::Gray)),
            status_area,
        );
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), input_area);

    let composer_hint = if state.active_provider.is_none() {
        if state.busy {
            "Tab focus • ? help • /help slash commands • Enter queues • Shift+Enter newline • arrows edit • placeholder model"
        } else {
            "Tab focus • ? help • /help slash commands • Enter submit • Shift+Enter newline • arrows edit • placeholder model"
        }
    } else {
        if state.busy {
            "Tab focus • ? help • /help slash commands • Enter queues • Shift+Enter newline • arrows edit • Alt+P/N history"
        } else {
            "Tab focus • ? help • /help slash commands • Enter submit • Shift+Enter newline • arrows edit • Alt+P/N history"
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            composer_hint,
            subtle_text(Color::DarkGray),
        ))),
        hint_area,
    );

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

fn tui_busy_status_line(state: &TuiState) -> Line<'static> {
    let spinner = match state
        .busy_started_at
        .map(|started_at| started_at.elapsed().as_millis() / 125)
        .unwrap_or(0)
        % 4
    {
        0 => "⠋",
        1 => "⠙",
        2 => "⠹",
        _ => "⠸",
    };
    let elapsed = state
        .busy_started_at
        .map(|started_at| format_elapsed(started_at.elapsed().as_secs()))
        .unwrap_or_else(|| "0s".to_string());

    let active = state
        .activity_entries
        .iter()
        .rev()
        .find(|entry| matches!(entry.tone, ActivityTone::Running))
        .map(|entry| entry.title.as_str())
        .unwrap_or("planning");

    Line::from(vec![
        Span::styled(
            format!("{spinner} "),
            Style::default().fg(Color::LightGreen),
        ),
        Span::styled("Working", Style::default().fg(Color::White)),
        Span::styled(format!(" • {elapsed}"), subtle_text(Color::DarkGray)),
        Span::styled(" • ", subtle_text(Color::DarkGray)),
        Span::styled(active.to_string(), Style::default().fg(Color::Gray)),
        Span::styled(" • ", subtle_text(Color::DarkGray)),
        Span::styled("Enter queues follow-up", subtle_text(Color::DarkGray)),
        if state.queued_prompts.is_empty() {
            Span::styled(
                " • Esc exits after completion",
                subtle_text(Color::DarkGray),
            )
        } else {
            Span::styled(
                format!(" • queued {}", state.queued_prompts.len()),
                Style::default().fg(Color::LightYellow),
            )
        },
    ])
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
            if matches!(
                message.role,
                zetta_protocol::MessageRole::Assistant | zetta_protocol::MessageRole::System
            ) {
                lines.extend(render_markdown_styled_lines(&content, body_style));
            } else {
                for line in content.lines() {
                    lines.push(StyledTextLine::styled(format!("  {line}"), body_style));
                }
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
        lines.extend(render_markdown_styled_lines(
            &state.pending_assistant,
            Style::default().fg(Color::White),
        ));
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

    lines.push(kv_line(
        "follow",
        if state.transcript_scroll == 0 {
            "live"
        } else {
            "paused"
        },
    ));

    if state.transcript_unseen > 0 {
        lines.push(kv_line("new msgs", &state.transcript_unseen.to_string()));
    }
    if state.activity_unseen > 0 {
        lines.push(kv_line("new act", &state.activity_unseen.to_string()));
    }
    if !state.queued_prompts.is_empty() {
        lines.push(kv_line("queued", &state.queued_prompts.len().to_string()));
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
                    if state.busy {
                        "Current turn is still running • Enter queues a follow-up • /help shows slash commands • Shift+Enter newline • ←/→ edit"
                    } else {
                        "Tab focuses panes • Enter sends • /help shows slash commands • Shift+Enter newline • ←/→ edit • Alt+P/N recalls prior prompts"
                    },
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            0,
            0,
        );
    }

    let cursor = clamp_cursor_boundary(&state.input, state.input_cursor);
    let before_cursor = &state.input[..cursor];
    let wrapped = wrap_plain_lines(&split_text_lines(&state.input), width.max(1));
    let before_wrapped = wrap_plain_lines(&split_text_lines(before_cursor), width.max(1));
    let absolute_cursor_line = before_wrapped.len().saturating_sub(1);
    let cursor_col = before_wrapped
        .last()
        .map(|line| display_width(line))
        .unwrap_or(0);
    let start = absolute_cursor_line
        .saturating_add(1)
        .saturating_sub(height.max(1))
        .min(wrapped.len().saturating_sub(height.max(1)));
    let end = (start + height.max(1)).min(wrapped.len());
    let visible = wrapped[start..end].to_vec();
    let cursor_line = absolute_cursor_line.saturating_sub(start);
    let rendered = visible
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::White))))
        .collect::<Vec<_>>();
    (rendered, cursor_line, cursor_col)
}

pub(crate) fn format_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3600 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!(
        "{}h {:02}m {:02}s",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

pub(crate) fn render_markdown_styled_lines(input: &str, base_style: Style) -> Vec<StyledTextLine> {
    if input.trim().is_empty() {
        return vec![StyledTextLine::styled("  ", base_style)];
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = MarkdownParser::new_ext(input, options);
    let mut renderer = MarkdownRenderer::new(base_style);
    for event in parser {
        renderer.handle_event(event);
    }
    renderer.finish()
}

#[derive(Clone, Copy)]
struct MarkdownListContext {
    ordered: bool,
    next_index: usize,
}

struct MarkdownRenderer {
    lines: Vec<StyledTextLine>,
    current: String,
    base_style: Style,
    current_style: Style,
    list_stack: Vec<MarkdownListContext>,
    blockquote_depth: usize,
    heading_level: Option<HeadingLevel>,
    in_code_block: bool,
    code_lang: Option<String>,
    code_buffer: String,
}

impl MarkdownRenderer {
    fn new(base_style: Style) -> Self {
        Self {
            lines: Vec::new(),
            current: String::new(),
            base_style,
            current_style: base_style,
            list_stack: Vec::new(),
            blockquote_depth: 0,
            heading_level: None,
            in_code_block: false,
            code_lang: None,
            code_buffer: String::new(),
        }
    }

    fn finish(mut self) -> Vec<StyledTextLine> {
        self.flush_current();
        if self.lines.is_empty() {
            self.lines
                .push(StyledTextLine::styled("  ", self.base_style));
        }
        self.lines
    }

    fn handle_event(&mut self, event: MarkdownEvent<'_>) {
        if self.in_code_block {
            match event {
                MarkdownEvent::End(TagEnd::CodeBlock) => {
                    self.flush_code_block();
                    self.in_code_block = false;
                    self.code_lang = None;
                }
                MarkdownEvent::Text(text) | MarkdownEvent::Code(text) => {
                    self.code_buffer.push_str(text.as_ref());
                }
                MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => self.code_buffer.push('\n'),
                _ => {}
            }
            return;
        }

        match event {
            MarkdownEvent::Start(tag) => self.start_tag(tag),
            MarkdownEvent::End(tag_end) => self.end_tag(tag_end),
            MarkdownEvent::Text(text) => self.push_text(text.as_ref()),
            MarkdownEvent::Code(code) => self.push_text(&format!("`{code}`")),
            MarkdownEvent::SoftBreak => self.flush_current(),
            MarkdownEvent::HardBreak => {
                self.flush_current();
                self.lines.push(StyledTextLine::plain(String::new()));
            }
            MarkdownEvent::Rule => {
                self.flush_current();
                self.lines.push(StyledTextLine::styled(
                    "  ─────────────────────────────",
                    subtle_text(Color::DarkGray),
                ));
            }
            MarkdownEvent::Html(html) | MarkdownEvent::InlineHtml(html) => {
                self.push_text(html.as_ref())
            }
            MarkdownEvent::InlineMath(text) => self.push_text(&format!("${text}$")),
            MarkdownEvent::DisplayMath(text) => {
                self.flush_current();
                self.lines.push(StyledTextLine::styled(
                    format!("  $$ {text} $$"),
                    Style::default().fg(Color::LightMagenta),
                ));
                self.lines.push(StyledTextLine::plain(String::new()));
            }
            MarkdownEvent::FootnoteReference(text) => {
                self.push_text(&format!("[^{text}]"));
            }
            MarkdownEvent::TaskListMarker(checked) => {
                self.ensure_prefix();
                self.current.push_str(if checked { "[x] " } else { "[ ] " });
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.current_style = self.heading_style_or_base();
            }
            Tag::Heading { level, .. } => {
                self.flush_current();
                self.heading_level = Some(level);
                self.current_style = markdown_heading_style(level, self.base_style);
            }
            Tag::BlockQuote(_) => {
                self.flush_current();
                self.blockquote_depth += 1;
                self.current_style = Style::default().fg(Color::LightCyan);
            }
            Tag::List(start) => {
                self.flush_current();
                self.list_stack.push(MarkdownListContext {
                    ordered: start.is_some(),
                    next_index: start.unwrap_or(1) as usize,
                });
            }
            Tag::Item => {
                self.flush_current();
            }
            Tag::CodeBlock(kind) => {
                self.flush_current();
                self.in_code_block = true;
                self.code_lang = match kind {
                    CodeBlockKind::Indented => None,
                    CodeBlockKind::Fenced(lang) => {
                        let lang = lang.trim();
                        (!lang.is_empty()).then(|| lang.to_string())
                    }
                };
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag_end: TagEnd) {
        match tag_end {
            TagEnd::Paragraph => {
                self.flush_current();
                self.lines.push(StyledTextLine::plain(String::new()));
                self.current_style = self.base_style;
            }
            TagEnd::Heading(_) => {
                self.flush_current();
                self.lines.push(StyledTextLine::plain(String::new()));
                self.heading_level = None;
                self.current_style = self.base_style;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_current();
                self.lines.push(StyledTextLine::plain(String::new()));
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.current_style = self.base_style;
            }
            TagEnd::List(_) => {
                self.flush_current();
                self.list_stack.pop();
            }
            TagEnd::Item => {
                self.flush_current();
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.ensure_prefix();
        self.current.push_str(text);
    }

    fn ensure_prefix(&mut self) {
        if !self.current.is_empty() {
            return;
        }

        self.current.push_str("  ");

        if self.blockquote_depth > 0 {
            for _ in 0..self.blockquote_depth {
                self.current.push_str("│ ");
            }
        }

        if let Some(list) = self.list_stack.last_mut() {
            if list.ordered {
                self.current.push_str(&format!("{}. ", list.next_index));
                list.next_index += 1;
            } else {
                self.current.push_str("• ");
            }
        }
    }

    fn flush_current(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let style = self.heading_style_or_base();
        let text = std::mem::take(&mut self.current);
        self.lines.push(StyledTextLine::styled(text, style));
    }

    fn flush_code_block(&mut self) {
        let code_style = Style::default().fg(Color::LightGreen);
        let fence_style = subtle_text(Color::DarkGray);
        let lang = self.code_lang.as_deref().unwrap_or("text");
        self.lines
            .push(StyledTextLine::styled(format!("  ```{lang}"), fence_style));
        for line in self.code_buffer.lines() {
            self.lines
                .push(StyledTextLine::styled(format!("    {line}"), code_style));
        }
        if self.code_buffer.ends_with('\n') {
            self.lines.push(StyledTextLine::styled("    ", code_style));
        }
        self.lines
            .push(StyledTextLine::styled("  ```", fence_style));
        self.lines.push(StyledTextLine::plain(String::new()));
        self.code_buffer.clear();
    }

    fn heading_style_or_base(&self) -> Style {
        self.heading_level
            .map(|level| markdown_heading_style(level, self.base_style))
            .unwrap_or(self.current_style)
    }
}

fn markdown_heading_style(level: HeadingLevel, base_style: Style) -> Style {
    let color = match level {
        HeadingLevel::H1 => Color::LightBlue,
        HeadingLevel::H2 => Color::LightCyan,
        HeadingLevel::H3 => Color::LightGreen,
        HeadingLevel::H4 => Color::White,
        HeadingLevel::H5 => Color::Gray,
        HeadingLevel::H6 => Color::DarkGray,
    };
    base_style.fg(color).add_modifier(Modifier::BOLD)
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

pub(crate) fn wrap_plain_lines(lines: &[String], width: usize) -> Vec<String> {
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

pub(crate) fn split_text_lines(input: &str) -> Vec<String> {
    let mut lines = input.lines().map(ToString::to_string).collect::<Vec<_>>();
    if input.ends_with('\n') {
        lines.push(String::new());
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub(crate) fn clamp_cursor_boundary(input: &str, cursor: usize) -> usize {
    if cursor >= input.len() {
        return input.len();
    }
    let mut clamped = cursor;
    while clamped > 0 && !input.is_char_boundary(clamped) {
        clamped -= 1;
    }
    clamped
}

pub(crate) fn previous_char_boundary(input: &str, cursor: usize) -> Option<usize> {
    if cursor == 0 {
        return None;
    }
    let mut index = clamp_cursor_boundary(input, cursor).saturating_sub(1);
    while index > 0 && !input.is_char_boundary(index) {
        index -= 1;
    }
    Some(index)
}

pub(crate) fn next_char_boundary(input: &str, cursor: usize) -> Option<usize> {
    let mut index = clamp_cursor_boundary(input, cursor);
    if index >= input.len() {
        return None;
    }
    index += 1;
    while index < input.len() && !input.is_char_boundary(index) {
        index += 1;
    }
    Some(index)
}

pub(crate) fn line_start_boundary(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor_boundary(input, cursor);
    input[..cursor].rfind('\n').map(|idx| idx + 1).unwrap_or(0)
}

pub(crate) fn line_end_boundary(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor_boundary(input, cursor);
    input[cursor..]
        .find('\n')
        .map(|offset| cursor + offset)
        .unwrap_or(input.len())
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
            if runtime.state.transcript_scroll > 0 {
                runtime.state.transcript_unseen = runtime.state.transcript_unseen.saturating_add(1);
            }
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
        if runtime.state.transcript_scroll > 0 {
            runtime.state.transcript_unseen = runtime.state.transcript_unseen.saturating_add(1);
        }
        runtime.render()?;
        Ok(())
    }

    fn on_message_end(&mut self) -> Result<()> {
        let mut runtime = self.runtime.lock().expect("tui runtime");
        runtime.render()?;
        Ok(())
    }
}

pub(crate) fn display_width(input: &str) -> usize {
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
