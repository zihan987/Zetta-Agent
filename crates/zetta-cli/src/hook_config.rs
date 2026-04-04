use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use zetta_protocol::SessionId;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistentHookConfig {
    #[serde(default)]
    pub denied_tools: BTreeMap<String, String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug)]
pub enum HookScope {
    Global,
    Session(SessionId),
}

pub struct HookConfigStore {
    config_dir: PathBuf,
}

impl HookConfigStore {
    #[must_use]
    pub fn new(config_dir: impl AsRef<Path>) -> Self {
        Self {
            config_dir: config_dir.as_ref().to_path_buf(),
        }
    }

    pub fn load_global(&self) -> Result<PersistentHookConfig> {
        load_config(&self.global_path())
    }

    pub fn save_global(&self, config: &PersistentHookConfig) -> Result<()> {
        save_config(&self.global_path(), config)
    }

    pub fn load_session(&self, session_id: SessionId) -> Result<PersistentHookConfig> {
        load_config(&self.session_path(session_id))
    }

    pub fn save_session(&self, session_id: SessionId, config: &PersistentHookConfig) -> Result<()> {
        save_config(&self.session_path(session_id), config)
    }

    pub fn clear_global(&self) -> Result<()> {
        let path = self.global_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn clear_session(&self, session_id: SessionId) -> Result<()> {
        let path = self.session_path(session_id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn global_path(&self) -> PathBuf {
        self.config_dir.join("hooks.json")
    }

    fn session_path(&self, session_id: SessionId) -> PathBuf {
        self.config_dir
            .join("session-hooks")
            .join(format!("{session_id}.json"))
    }
}

pub fn merge_hook_configs(
    configs: impl IntoIterator<Item = PersistentHookConfig>,
) -> PersistentHookConfig {
    let mut merged = PersistentHookConfig::default();

    for config in configs {
        merged.denied_tools.extend(config.denied_tools);
        merged.tags.extend(config.tags);
        merged.metadata.extend(config.metadata);
    }

    dedupe_tags(&mut merged.tags);

    merged
}

fn dedupe_tags(tags: &mut Vec<String>) {
    let mut seen = HashSet::new();
    tags.retain(|tag| seen.insert(tag.clone()));
}

fn load_config(path: &Path) -> Result<PersistentHookConfig> {
    if !path.exists() {
        return Ok(PersistentHookConfig::default());
    }

    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
}

fn save_config(path: &Path, config: &PersistentHookConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(config)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{merge_hook_configs, PersistentHookConfig};

    #[test]
    fn merge_overwrites_metadata_and_reasons_and_dedupes_tags() {
        let mut global = PersistentHookConfig::default();
        global
            .denied_tools
            .insert("bash".to_string(), "global deny".to_string());
        global.tags.push("trusted".to_string());
        global
            .metadata
            .insert("owner".to_string(), "ops".to_string());

        let mut session = PersistentHookConfig::default();
        session
            .denied_tools
            .insert("bash".to_string(), "session deny".to_string());
        session.tags.push("trusted".to_string());
        session.tags.push("interactive".to_string());
        session
            .metadata
            .insert("owner".to_string(), "codex".to_string());

        let merged = merge_hook_configs([global, session]);
        assert_eq!(
            merged.denied_tools.get("bash").map(String::as_str),
            Some("session deny")
        );
        assert_eq!(
            merged.tags,
            vec!["trusted".to_string(), "interactive".to_string()]
        );
        assert_eq!(
            merged.metadata.get("owner").map(String::as_str),
            Some("codex")
        );
    }
}
