use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use zetta_core::tool::{PermissionMode, PermissionRules};
use zetta_protocol::SessionId;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistentPermissionConfig {
    pub mode: Option<PermissionMode>,
    #[serde(default)]
    pub rules: PermissionRules,
}

#[derive(Clone, Copy, Debug)]
pub enum PermissionScope {
    Global,
    Session(SessionId),
}

pub struct PermissionConfigStore {
    config_dir: PathBuf,
}

impl PermissionConfigStore {
    #[must_use]
    pub fn new(config_dir: impl AsRef<Path>) -> Self {
        Self {
            config_dir: config_dir.as_ref().to_path_buf(),
        }
    }

    pub fn load_global(&self) -> Result<PersistentPermissionConfig> {
        load_config(&self.global_path())
    }

    pub fn save_global(&self, config: &PersistentPermissionConfig) -> Result<()> {
        save_config(&self.global_path(), config)
    }

    pub fn load_session(&self, session_id: SessionId) -> Result<PersistentPermissionConfig> {
        load_config(&self.session_path(session_id))
    }

    pub fn save_session(
        &self,
        session_id: SessionId,
        config: &PersistentPermissionConfig,
    ) -> Result<()> {
        save_config(&self.session_path(session_id), config)
    }

    pub fn clear_session(&self, session_id: SessionId) -> Result<()> {
        let path = self.session_path(session_id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn clear_global(&self) -> Result<()> {
        let path = self.global_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn global_path(&self) -> PathBuf {
        self.config_dir.join("permissions.json")
    }

    fn session_path(&self, session_id: SessionId) -> PathBuf {
        self.config_dir
            .join("session-permissions")
            .join(format!("{session_id}.json"))
    }
}

pub fn merge_permission_configs(
    configs: impl IntoIterator<Item = PersistentPermissionConfig>,
) -> PersistentPermissionConfig {
    let mut merged = PersistentPermissionConfig::default();

    for config in configs {
        if config.mode.is_some() {
            merged.mode = config.mode;
        }

        merged
            .rules
            .readable_roots
            .extend(config.rules.readable_roots.into_iter());
        merged
            .rules
            .writable_roots
            .extend(config.rules.writable_roots.into_iter());
        merged
            .rules
            .allowed_tools
            .extend(config.rules.allowed_tools.into_iter());
        merged
            .rules
            .denied_tools
            .extend(config.rules.denied_tools.into_iter());
    }

    dedupe_paths(&mut merged.rules.readable_roots);
    dedupe_paths(&mut merged.rules.writable_roots);

    merged
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

fn load_config(path: &Path) -> Result<PersistentPermissionConfig> {
    if !path.exists() {
        return Ok(PersistentPermissionConfig::default());
    }

    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
}

fn save_config(path: &Path, config: &PersistentPermissionConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(config)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zetta_core::tool::PermissionMode;

    use super::{merge_permission_configs, PersistentPermissionConfig};

    #[test]
    fn merge_uses_last_mode_and_unions_rules() {
        let mut global = PersistentPermissionConfig {
            mode: Some(PermissionMode::WorkspaceWrite),
            ..PersistentPermissionConfig::default()
        };
        global.rules.readable_roots.push(PathBuf::from("/a"));
        global.rules.allowed_tools.insert("file_read".to_string());

        let mut session = PersistentPermissionConfig {
            mode: Some(PermissionMode::ReadOnly),
            ..PersistentPermissionConfig::default()
        };
        session.rules.readable_roots.push(PathBuf::from("/b"));
        session.rules.denied_tools.insert("bash".to_string());

        let merged = merge_permission_configs([global, session]);
        assert_eq!(merged.mode, Some(PermissionMode::ReadOnly));
        assert_eq!(merged.rules.readable_roots.len(), 2);
        assert!(merged.rules.allowed_tools.contains("file_read"));
        assert!(merged.rules.denied_tools.contains("bash"));
    }
}
