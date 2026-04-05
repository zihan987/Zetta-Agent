use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use zetta_protocol::{EngineEvent, Message, MessageRole, SessionSnapshot, TurnRequest};

use crate::hook::{HookBus, HookDispatch, HookErrorRecord, HookEvent, HookMutation, HookPlanKind};
use crate::model::{
    encode_tool_denied_message, encode_tool_failed_message, encode_tool_invalid_call_message,
    encode_tool_result_message, ModelClient, ModelStreamSink, PlannedTurn,
};
use crate::session::SessionStore;
use crate::tool::{ToolInvocationError, ToolRegistry, ToolUseContext};

const MAX_MODEL_STEPS: usize = 8;

pub trait EngineEventSink {
    fn on_event(&mut self, event: &EngineEvent) -> Result<()>;
}

pub struct AgentEngine {
    model: Arc<dyn ModelClient>,
    sessions: Arc<dyn SessionStore>,
    tools: ToolRegistry,
    tool_context: ToolUseContext,
    hooks: HookBus,
}

pub struct RunTurnOutput {
    pub events: Vec<EngineEvent>,
    pub session: SessionSnapshot,
    pub hook_failures: Vec<HookErrorRecord>,
}

impl AgentEngine {
    #[must_use]
    pub fn new(
        model: Arc<dyn ModelClient>,
        sessions: Arc<dyn SessionStore>,
        tools: ToolRegistry,
        tool_context: ToolUseContext,
        hooks: HookBus,
    ) -> Self {
        Self {
            model,
            sessions,
            tools,
            tool_context,
            hooks,
        }
    }

    pub async fn run_turn(&self, request: TurnRequest) -> Result<RunTurnOutput> {
        self.run_turn_with_sinks(request, None, None).await
    }

    pub async fn run_turn_with_model_sink(
        &self,
        request: TurnRequest,
        mut model_sink: Option<&mut dyn ModelStreamSink>,
    ) -> Result<RunTurnOutput> {
        self.run_turn_with_sinks(request, model_sink.take(), None)
            .await
    }

    pub async fn run_turn_with_sinks(
        &self,
        request: TurnRequest,
        mut model_sink: Option<&mut dyn ModelStreamSink>,
        mut event_sink: Option<&mut dyn EngineEventSink>,
    ) -> Result<RunTurnOutput> {
        let mut hook_failures = Vec::new();
        let session_id = request.session_id.unwrap_or_default();
        let loaded = self.sessions.load(&session_id).await?;
        let is_new = loaded.is_none();
        let mut session = loaded.unwrap_or_else(|| SessionSnapshot::new(session_id));

        let mut events = Vec::new();
        self.push_event(
            &mut events,
            EngineEvent::SessionLoaded { session_id, is_new },
            &mut event_sink,
        )?;
        self.apply_hook_dispatch(
            &mut session,
            self.hooks
                .emit(HookEvent::SessionLoaded { session_id, is_new })
                .await,
            &mut hook_failures,
        );

        let user_message = Message::new(MessageRole::User, request.prompt);
        session.messages.push(user_message.clone());
        session.updated_at = Utc::now();
        self.push_event(
            &mut events,
            EngineEvent::UserMessagePersisted {
                message: user_message,
            },
            &mut event_sink,
        )?;
        let mut completed = false;
        for _ in 0..MAX_MODEL_STEPS {
            let message_count = session.messages.len();
            self.apply_hook_dispatch(
                &mut session,
                self.hooks
                    .emit(HookEvent::BeforeModelPlan {
                        session_id,
                        message_count,
                    })
                    .await,
                &mut hook_failures,
            );

            let planned = match model_sink {
                Some(ref mut sink) => {
                    self.model
                        .plan_turn_with_sink(&session, Some(&mut **sink))
                        .await?
                }
                None => self.model.plan_turn(&session).await?,
            };

            match planned {
                PlannedTurn::AssistantMessage(content) => {
                    self.apply_hook_dispatch(
                        &mut session,
                        self.hooks
                            .emit(HookEvent::AfterModelPlan {
                                session_id,
                                plan: HookPlanKind::AssistantMessage,
                                tool_name: None,
                            })
                            .await,
                        &mut hook_failures,
                    );
                    let assistant_message = Message::new(MessageRole::Assistant, content);
                    session.messages.push(assistant_message.clone());
                    self.push_event(
                        &mut events,
                        EngineEvent::AssistantMessagePersisted {
                            message: assistant_message,
                        },
                        &mut event_sink,
                    )?;
                    completed = true;
                    break;
                }
                PlannedTurn::ToolCall(call) => {
                    let tool_name = call.name.clone();
                    self.apply_hook_dispatch(
                        &mut session,
                        self.hooks
                            .emit(HookEvent::AfterModelPlan {
                                session_id,
                                plan: HookPlanKind::ToolCall,
                                tool_name: Some(tool_name.clone()),
                            })
                            .await,
                        &mut hook_failures,
                    );
                    self.push_event(
                        &mut events,
                        EngineEvent::ToolCallRequested { call: call.clone() },
                        &mut event_sink,
                    )?;
                    let before_tool_dispatch = self
                        .hooks
                        .emit(HookEvent::BeforeToolCall {
                            session_id,
                            tool_name: tool_name.clone(),
                        })
                        .await;
                    if let Some(reason) = before_tool_dispatch.mutation.deny_reason.clone() {
                        self.apply_hook_dispatch(
                            &mut session,
                            before_tool_dispatch,
                            &mut hook_failures,
                        );
                        let tool_message = Message::new(
                            MessageRole::Tool,
                            encode_tool_denied_message(&tool_name, &reason)?,
                        );
                        session.messages.push(tool_message);
                        self.push_event(
                            &mut events,
                            EngineEvent::ToolCallDenied {
                                call,
                                reason: reason.clone(),
                            },
                            &mut event_sink,
                        )?;
                        self.apply_hook_dispatch(
                            &mut session,
                            self.hooks
                                .emit(HookEvent::ToolDenied {
                                    session_id,
                                    tool_name,
                                    reason,
                                })
                                .await,
                            &mut hook_failures,
                        );
                        continue;
                    }
                    self.apply_hook_dispatch(
                        &mut session,
                        before_tool_dispatch,
                        &mut hook_failures,
                    );

                    match self.tools.invoke(&call, &self.tool_context).await {
                        Ok(result) => {
                            let tool_message = Message::new(
                                MessageRole::Tool,
                                encode_tool_result_message(&result.name, &result.output)?,
                            );
                            session.messages.push(tool_message);
                            self.push_event(
                                &mut events,
                                EngineEvent::ToolCallCompleted {
                                    result: result.clone(),
                                },
                                &mut event_sink,
                            )?;
                            self.apply_hook_dispatch(
                                &mut session,
                                self.hooks
                                    .emit(HookEvent::AfterToolCall {
                                        session_id,
                                        tool_name: result.name.clone(),
                                    })
                                    .await,
                                &mut hook_failures,
                            );
                        }
                        Err(ToolInvocationError::Denied { reason }) => {
                            let denied_reason = reason.clone();
                            let tool_message = Message::new(
                                MessageRole::Tool,
                                encode_tool_denied_message(&tool_name, &reason)?,
                            );
                            session.messages.push(tool_message);
                            self.push_event(
                                &mut events,
                                EngineEvent::ToolCallDenied { call, reason },
                                &mut event_sink,
                            )?;
                            self.apply_hook_dispatch(
                                &mut session,
                                self.hooks
                                    .emit(HookEvent::ToolDenied {
                                        session_id,
                                        tool_name: tool_name.clone(),
                                        reason: denied_reason,
                                    })
                                    .await,
                                &mut hook_failures,
                            );
                            continue;
                        }
                        Err(ToolInvocationError::Failed(error)) => {
                            let error_text = error.to_string();
                            let tool_message = Message::new(
                                MessageRole::Tool,
                                encode_tool_failed_message(&tool_name, &error_text)?,
                            );
                            session.messages.push(tool_message);
                            self.push_event(
                                &mut events,
                                EngineEvent::ToolCallFailed {
                                    call,
                                    error: error_text.clone(),
                                },
                                &mut event_sink,
                            )?;
                            self.apply_hook_dispatch(
                                &mut session,
                                self.hooks
                                    .emit(HookEvent::ToolFailed {
                                        session_id,
                                        tool_name,
                                        error: error_text,
                                    })
                                    .await,
                                &mut hook_failures,
                            );
                            continue;
                        }
                    }
                }
                PlannedTurn::InvalidToolCall { raw, error } => {
                    self.apply_hook_dispatch(
                        &mut session,
                        self.hooks
                            .emit(HookEvent::AfterModelPlan {
                                session_id,
                                plan: HookPlanKind::ToolCall,
                                tool_name: None,
                            })
                            .await,
                        &mut hook_failures,
                    );
                    let tool_message = Message::new(
                        MessageRole::Tool,
                        encode_tool_invalid_call_message(&raw, &error)?,
                    );
                    session.messages.push(tool_message);
                    continue;
                }
            }
        }

        if !completed {
            let assistant_message = Message::new(
                MessageRole::Assistant,
                format!("Stopped after reaching the max planning steps ({MAX_MODEL_STEPS})."),
            );
            session.messages.push(assistant_message.clone());
            self.push_event(
                &mut events,
                EngineEvent::AssistantMessagePersisted {
                    message: assistant_message,
                },
                &mut event_sink,
            )?;
        }

        session.updated_at = Utc::now();
        let message_count = session.messages.len();
        self.apply_hook_dispatch(
            &mut session,
            self.hooks
                .emit(HookEvent::BeforeSessionSave {
                    session_id,
                    message_count,
                })
                .await,
            &mut hook_failures,
        );
        self.sessions.save(&session).await?;
        let message_count = session.messages.len();
        self.apply_hook_dispatch(
            &mut session,
            self.hooks
                .emit(HookEvent::AfterSessionSave {
                    session_id,
                    message_count,
                })
                .await,
            &mut hook_failures,
        );
        self.push_event(
            &mut events,
            EngineEvent::TurnFinished { session_id },
            &mut event_sink,
        )?;

        Ok(RunTurnOutput {
            events,
            session,
            hook_failures,
        })
    }

    fn push_event(
        &self,
        events: &mut Vec<EngineEvent>,
        event: EngineEvent,
        event_sink: &mut Option<&mut dyn EngineEventSink>,
    ) -> Result<()> {
        if let Some(sink) = event_sink.as_deref_mut() {
            sink.on_event(&event)?;
        }
        events.push(event);
        Ok(())
    }

    fn apply_hook_dispatch(
        &self,
        session: &mut SessionSnapshot,
        dispatch: HookDispatch,
        hook_failures: &mut Vec<HookErrorRecord>,
    ) {
        hook_failures.extend(dispatch.failures);
        self.apply_hook_mutation(session, dispatch.mutation);
    }

    fn apply_hook_mutation(&self, session: &mut SessionSnapshot, mutation: HookMutation) {
        for tag in mutation.session_tags {
            if !session.tags.contains(&tag) {
                session.tags.push(tag);
            }
        }

        for (key, value) in mutation.session_metadata {
            session.metadata.insert(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::tempdir;
    use zetta_protocol::{EngineEvent, MessageRole, TurnRequest};

    use std::collections::BTreeMap;

    use crate::hook::{DenyToolHook, HookBus, HookEvent, RecordingHook, SessionAnnotatingHook};
    use crate::model::{ModelClient, PlannedTurn, RuleBasedModelClient};
    use crate::session::FileSessionStore;
    use crate::tool::{
        EchoTool, PermissionMode, PermissionPolicy, PermissionRules, Tool, ToolCapability,
        ToolInvocationError, ToolRegistry, ToolUseContext,
    };

    use super::AgentEngine;

    struct MultiStepModelClient;
    struct AlwaysFailTool;
    struct InvalidThenRecoverModel;

    #[async_trait]
    impl ModelClient for MultiStepModelClient {
        async fn plan_turn(
            &self,
            session: &zetta_protocol::SessionSnapshot,
        ) -> Result<PlannedTurn> {
            let tool_count = session
                .messages
                .iter()
                .filter(|message| matches!(message.role, MessageRole::Tool))
                .count();

            match tool_count {
                0 => Ok(PlannedTurn::ToolCall(zetta_protocol::ToolCall {
                    name: "echo".to_string(),
                    input: json!({ "text": "first step" }),
                })),
                1 => Ok(PlannedTurn::ToolCall(zetta_protocol::ToolCall {
                    name: "echo".to_string(),
                    input: json!({ "text": "second step" }),
                })),
                _ => Ok(PlannedTurn::AssistantMessage(
                    "multi-step complete".to_string(),
                )),
            }
        }
    }

    #[async_trait]
    impl Tool for AlwaysFailTool {
        fn name(&self) -> &'static str {
            "always_fail"
        }

        fn description(&self) -> &'static str {
            "Always fails for engine recovery tests."
        }

        fn capability(&self) -> ToolCapability {
            ToolCapability::Write
        }

        async fn invoke(
            &self,
            _input: serde_json::Value,
            _context: &ToolUseContext,
        ) -> Result<serde_json::Value, ToolInvocationError> {
            Err(ToolInvocationError::Failed(anyhow!("intentional failure")))
        }
    }

    #[async_trait]
    impl ModelClient for InvalidThenRecoverModel {
        async fn plan_turn(
            &self,
            session: &zetta_protocol::SessionSnapshot,
        ) -> Result<PlannedTurn> {
            if matches!(
                session.messages.last().map(|message| message.role),
                Some(MessageRole::Tool)
            ) {
                let tool_count = session
                    .messages
                    .iter()
                    .filter(|message| matches!(message.role, MessageRole::Tool))
                    .count();
                let invalid_count = session
                    .messages
                    .iter()
                    .filter(|message| {
                        matches!(message.role, MessageRole::Tool)
                            && message.content.contains("\"status\": \"invalid_call\"")
                    })
                    .count();

                return match (tool_count, invalid_count) {
                    (1, 1) => Ok(PlannedTurn::ToolCall(zetta_protocol::ToolCall {
                        name: "echo".to_string(),
                        input: json!({ "text": "recovered" }),
                    })),
                    _ => Ok(PlannedTurn::AssistantMessage(
                        "recovered from invalid call".to_string(),
                    )),
                };
            }

            Ok(PlannedTurn::InvalidToolCall {
                raw: "/tool file_read_lines src/main.rs:oops".to_string(),
                error: "expected `path:start-end` for `file_read_lines`".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn tool_calls_are_persisted_to_session() -> anyhow::Result<()> {
        let temp_dir = tempdir().expect("temp dir");
        let store = Arc::new(FileSessionStore::new(temp_dir.path()));
        let mut tools = ToolRegistry::default();
        tools.register(EchoTool);
        let tool_context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;

        let engine = AgentEngine::new(
            Arc::new(RuleBasedModelClient),
            store,
            tools,
            tool_context,
            HookBus::new(),
        );
        let result = engine
            .run_turn(TurnRequest {
                session_id: None,
                prompt: "/tool echo hello rust".to_string(),
            })
            .await
            .expect("run turn");

        let last = result.session.messages.last().expect("assistant message");
        assert_eq!(last.role, zetta_protocol::MessageRole::Assistant);
        assert!(last.content.contains("Tool `echo` returned:"));
        assert!(result
            .session
            .messages
            .iter()
            .any(|m| m.role == zetta_protocol::MessageRole::Tool));
        Ok(())
    }

    #[tokio::test]
    async fn hooks_receive_turn_lifecycle_events() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let store = Arc::new(FileSessionStore::new(temp_dir.path()));
        let mut tools = ToolRegistry::default();
        tools.register(EchoTool);
        let tool_context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let hook = RecordingHook::new();
        let recorded = hook.events();
        let mut hooks = HookBus::new();
        hooks.register(hook);

        let engine = AgentEngine::new(
            Arc::new(RuleBasedModelClient),
            store,
            tools,
            tool_context,
            hooks,
        );

        engine
            .run_turn(TurnRequest {
                session_id: None,
                prompt: "/tool echo hello rust".to_string(),
            })
            .await?;

        let events = recorded.lock().expect("recorded hook events");
        assert!(events.iter().any(|event| matches!(
            event,
            HookEvent::BeforeToolCall { tool_name, .. } if tool_name == "echo"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HookEvent::AfterToolCall { tool_name, .. } if tool_name == "echo"
        )));
        assert!(matches!(
            events.last(),
            Some(HookEvent::AfterSessionSave { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn hook_can_veto_tool_calls() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let store = Arc::new(FileSessionStore::new(temp_dir.path()));
        let mut tools = ToolRegistry::default();
        tools.register(EchoTool);
        let tool_context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let mut hooks = HookBus::new();
        hooks.register(DenyToolHook::new([(
            "echo".to_string(),
            "blocked by hook".to_string(),
        )]));

        let engine = AgentEngine::new(
            Arc::new(RuleBasedModelClient),
            store,
            tools,
            tool_context,
            hooks,
        );

        let result = engine
            .run_turn(TurnRequest {
                session_id: None,
                prompt: "/tool echo hello rust".to_string(),
            })
            .await?;

        assert!(result
            .events
            .iter()
            .any(|event| matches!(event, EngineEvent::ToolCallDenied { reason, .. } if reason == "blocked by hook")));
        assert!(result
            .session
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Tool));
        assert!(result
            .session
            .messages
            .last()
            .map(|message| message.content.contains("was denied"))
            .unwrap_or(false));
        Ok(())
    }

    #[tokio::test]
    async fn hook_can_attach_session_tags_and_metadata() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let store = Arc::new(FileSessionStore::new(temp_dir.path()));
        let mut tools = ToolRegistry::default();
        tools.register(EchoTool);
        let tool_context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;
        let mut hooks = HookBus::new();
        let mut metadata = BTreeMap::new();
        metadata.insert("owner".to_string(), "codex".to_string());
        hooks.register(SessionAnnotatingHook::new(
            vec!["trusted".to_string(), "hooked".to_string()],
            metadata,
        ));

        let engine = AgentEngine::new(
            Arc::new(RuleBasedModelClient),
            store,
            tools,
            tool_context,
            hooks,
        );

        let result = engine
            .run_turn(TurnRequest {
                session_id: None,
                prompt: "hello rust".to_string(),
            })
            .await?;

        assert!(result.session.tags.contains(&"trusted".to_string()));
        assert_eq!(
            result.session.metadata.get("owner").map(String::as_str),
            Some("codex")
        );
        Ok(())
    }

    #[tokio::test]
    async fn engine_can_execute_multiple_tool_steps_before_answering() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let store = Arc::new(FileSessionStore::new(temp_dir.path()));
        let mut tools = ToolRegistry::default();
        tools.register(EchoTool);
        let tool_context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;

        let engine = AgentEngine::new(
            Arc::new(MultiStepModelClient),
            store,
            tools,
            tool_context,
            HookBus::new(),
        );

        let result = engine
            .run_turn(TurnRequest {
                session_id: None,
                prompt: "do the sequence".to_string(),
            })
            .await?;

        let tool_results = result
            .events
            .iter()
            .filter(|event| matches!(event, EngineEvent::ToolCallCompleted { .. }))
            .count();
        assert_eq!(tool_results, 2);
        assert_eq!(
            result
                .session
                .messages
                .last()
                .map(|message| message.content.as_str()),
            Some("multi-step complete")
        );
        Ok(())
    }

    #[tokio::test]
    async fn tool_failures_are_recorded_as_tool_messages_then_summarized() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let store = Arc::new(FileSessionStore::new(temp_dir.path()));
        let mut tools = ToolRegistry::default();
        tools.register(AlwaysFailTool);
        let tool_context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;

        struct FailOnceModel;

        #[async_trait]
        impl ModelClient for FailOnceModel {
            async fn plan_turn(
                &self,
                session: &zetta_protocol::SessionSnapshot,
            ) -> Result<PlannedTurn> {
                if matches!(
                    session.messages.last().map(|message| message.role),
                    Some(MessageRole::Tool)
                ) {
                    return Ok(PlannedTurn::AssistantMessage("handled failure".to_string()));
                }

                Ok(PlannedTurn::ToolCall(zetta_protocol::ToolCall {
                    name: "always_fail".to_string(),
                    input: json!({}),
                }))
            }
        }

        let engine = AgentEngine::new(
            Arc::new(FailOnceModel),
            store,
            tools,
            tool_context,
            HookBus::new(),
        );

        let result = engine
            .run_turn(TurnRequest {
                session_id: None,
                prompt: "trigger failure".to_string(),
            })
            .await?;

        assert!(result
            .events
            .iter()
            .any(|event| matches!(event, EngineEvent::ToolCallFailed { error, .. } if error == "intentional failure")));
        assert!(result
            .session
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Tool));
        assert_eq!(
            result
                .session
                .messages
                .last()
                .map(|message| message.content.as_str()),
            Some("handled failure")
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_tool_calls_are_recorded_then_corrected() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let store = Arc::new(FileSessionStore::new(temp_dir.path()));
        let mut tools = ToolRegistry::default();
        tools.register(EchoTool);
        let tool_context = ToolUseContext::new(
            temp_dir.path(),
            PermissionPolicy::new(
                PermissionMode::WorkspaceWrite,
                temp_dir.path(),
                PermissionRules::default(),
            )?,
        )?;

        let engine = AgentEngine::new(
            Arc::new(InvalidThenRecoverModel),
            store,
            tools,
            tool_context,
            HookBus::new(),
        );

        let result = engine
            .run_turn(TurnRequest {
                session_id: None,
                prompt: "fix your tool call".to_string(),
            })
            .await?;

        let tool_messages = result
            .session
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .collect::<Vec<_>>();
        assert!(tool_messages
            .iter()
            .any(|message| message.content.contains("\"status\": \"invalid_call\"")));
        assert!(result
            .events
            .iter()
            .any(|event| matches!(event, EngineEvent::ToolCallCompleted { .. })));
        assert_eq!(
            result
                .session
                .messages
                .last()
                .map(|message| message.content.as_str()),
            Some("recovered from invalid call")
        );
        Ok(())
    }
}
