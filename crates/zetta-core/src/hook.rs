use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zetta_protocol::SessionId;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookPlanKind {
    AssistantMessage,
    ToolCall,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookEvent {
    SessionLoaded {
        session_id: SessionId,
        is_new: bool,
    },
    BeforeModelPlan {
        session_id: SessionId,
        message_count: usize,
    },
    AfterModelPlan {
        session_id: SessionId,
        plan: HookPlanKind,
        tool_name: Option<String>,
    },
    BeforeToolCall {
        session_id: SessionId,
        tool_name: String,
    },
    AfterToolCall {
        session_id: SessionId,
        tool_name: String,
    },
    ToolDenied {
        session_id: SessionId,
        tool_name: String,
        reason: String,
    },
    ToolFailed {
        session_id: SessionId,
        tool_name: String,
        error: String,
    },
    BeforeSessionSave {
        session_id: SessionId,
        message_count: usize,
    },
    AfterSessionSave {
        session_id: SessionId,
        message_count: usize,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookMutation {
    pub deny_reason: Option<String>,
    pub session_tags: Vec<String>,
    pub session_metadata: BTreeMap<String, String>,
}

impl HookMutation {
    pub fn merge_from(&mut self, other: HookMutation) {
        if self.deny_reason.is_none() {
            self.deny_reason = other.deny_reason;
        }

        self.session_tags.extend(other.session_tags);
        for (key, value) in other.session_metadata {
            self.session_metadata.insert(key, value);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookErrorRecord {
    pub handler_name: String,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookDispatch {
    pub failures: Vec<HookErrorRecord>,
    pub mutation: HookMutation,
}

#[async_trait]
pub trait HookHandler: Send + Sync {
    fn name(&self) -> &'static str;
    async fn handle(&self, event: &HookEvent) -> Result<HookMutation>;
}

#[derive(Clone, Default)]
pub struct HookBus {
    handlers: Vec<Arc<dyn HookHandler>>,
}

impl HookBus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T>(&mut self, handler: T)
    where
        T: HookHandler + 'static,
    {
        self.handlers.push(Arc::new(handler));
    }

    pub async fn emit(&self, event: HookEvent) -> HookDispatch {
        let mut dispatch = HookDispatch::default();

        for handler in &self.handlers {
            match handler.handle(&event).await {
                Ok(mutation) => dispatch.mutation.merge_from(mutation),
                Err(error) => dispatch.failures.push(HookErrorRecord {
                    handler_name: handler.name().to_string(),
                    error: error.to_string(),
                }),
            }
        }

        dispatch
    }
}

pub struct JsonlHook {
    path: PathBuf,
}

impl JsonlHook {
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

#[async_trait]
impl HookHandler for JsonlHook {
    fn name(&self) -> &'static str {
        "jsonl_hook"
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookMutation> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(event)?)?;
        Ok(HookMutation::default())
    }
}

#[derive(Clone, Default)]
pub struct RecordingHook {
    events: Arc<Mutex<Vec<HookEvent>>>,
}

impl RecordingHook {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn events(&self) -> Arc<Mutex<Vec<HookEvent>>> {
        Arc::clone(&self.events)
    }
}

#[async_trait]
impl HookHandler for RecordingHook {
    fn name(&self) -> &'static str {
        "recording_hook"
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookMutation> {
        self.events
            .lock()
            .expect("recording hook lock")
            .push(event.clone());
        Ok(HookMutation::default())
    }
}

pub struct DenyToolHook {
    denied_tools: HashMap<String, String>,
}

impl DenyToolHook {
    #[must_use]
    pub fn new<I>(items: I) -> Self
    where
        I: IntoIterator<Item = (String, String)>,
    {
        Self {
            denied_tools: items.into_iter().collect(),
        }
    }
}

#[async_trait]
impl HookHandler for DenyToolHook {
    fn name(&self) -> &'static str {
        "deny_tool_hook"
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookMutation> {
        let mut mutation = HookMutation::default();
        if let HookEvent::BeforeToolCall { tool_name, .. } = event {
            if let Some(reason) = self.denied_tools.get(tool_name) {
                mutation.deny_reason = Some(reason.clone());
            }
        }
        Ok(mutation)
    }
}

pub struct SessionAnnotatingHook {
    tags: Vec<String>,
    metadata: BTreeMap<String, String>,
}

impl SessionAnnotatingHook {
    #[must_use]
    pub fn new(tags: Vec<String>, metadata: BTreeMap<String, String>) -> Self {
        Self { tags, metadata }
    }
}

#[async_trait]
impl HookHandler for SessionAnnotatingHook {
    fn name(&self) -> &'static str {
        "session_annotating_hook"
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookMutation> {
        if matches!(event, HookEvent::SessionLoaded { .. }) {
            return Ok(HookMutation {
                deny_reason: None,
                session_tags: self.tags.clone(),
                session_metadata: self.metadata.clone(),
            });
        }

        Ok(HookMutation::default())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::{
        DenyToolHook, HookBus, HookEvent, HookMutation, JsonlHook, RecordingHook,
        SessionAnnotatingHook,
    };

    #[tokio::test]
    async fn recording_hook_captures_events() {
        let hook = RecordingHook::new();
        let events = hook.events();
        let mut bus = HookBus::new();
        bus.register(hook);

        let session_id = zetta_protocol::SessionId::new();
        let dispatch = bus
            .emit(HookEvent::SessionLoaded {
                session_id,
                is_new: true,
            })
            .await;

        assert!(dispatch.failures.is_empty());
        let captured = events.lock().expect("captured events");
        assert_eq!(captured.len(), 1);
        assert!(matches!(
            captured.first(),
            Some(HookEvent::SessionLoaded {
                session_id: _,
                is_new: true
            })
        ));
    }

    #[tokio::test]
    async fn jsonl_hook_writes_one_line_per_event() -> anyhow::Result<()> {
        let temp_dir = tempdir()?;
        let path = temp_dir.path().join("hooks.jsonl");
        let mut bus = HookBus::new();
        bus.register(JsonlHook::new(&path));

        bus.emit(HookEvent::BeforeSessionSave {
            session_id: zetta_protocol::SessionId::new(),
            message_count: 3,
        })
        .await;

        let contents = std::fs::read_to_string(path)?;
        assert_eq!(contents.lines().count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn deny_tool_hook_returns_deny_reason() {
        let mut bus = HookBus::new();
        bus.register(DenyToolHook::new([(
            "bash".to_string(),
            "hook veto".to_string(),
        )]));

        let dispatch = bus
            .emit(HookEvent::BeforeToolCall {
                session_id: zetta_protocol::SessionId::new(),
                tool_name: "bash".to_string(),
            })
            .await;

        assert_eq!(dispatch.mutation.deny_reason.as_deref(), Some("hook veto"));
    }

    #[tokio::test]
    async fn session_annotating_hook_adds_tags_and_metadata() {
        let mut metadata = BTreeMap::new();
        metadata.insert("owner".to_string(), "codex".to_string());
        let mut bus = HookBus::new();
        bus.register(SessionAnnotatingHook::new(
            vec!["trusted".to_string()],
            metadata.clone(),
        ));

        let dispatch = bus
            .emit(HookEvent::SessionLoaded {
                session_id: zetta_protocol::SessionId::new(),
                is_new: true,
            })
            .await;

        assert_eq!(
            dispatch.mutation,
            HookMutation {
                deny_reason: None,
                session_tags: vec!["trusted".to_string()],
                session_metadata: metadata,
            }
        );
    }
}
