use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistentProviderProfile {
    pub api_base: Option<String>,
    pub api_key_env: Option<String>,
    pub model_name: Option<String>,
    pub system_prompt: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistentProviderConfig {
    #[serde(default)]
    pub providers: BTreeMap<String, PersistentProviderProfile>,
}

pub struct ProviderConfigStore {
    config_dir: PathBuf,
}

impl ProviderConfigStore {
    #[must_use]
    pub fn new(config_dir: impl AsRef<Path>) -> Self {
        Self {
            config_dir: config_dir.as_ref().to_path_buf(),
        }
    }

    pub fn load(&self) -> Result<PersistentProviderConfig> {
        load_config(&self.path())
    }

    pub fn save(&self, config: &PersistentProviderConfig) -> Result<()> {
        save_config(&self.path(), config)
    }

    fn path(&self) -> PathBuf {
        self.config_dir.join("providers.json")
    }
}

fn load_config(path: &Path) -> Result<PersistentProviderConfig> {
    if !path.exists() {
        return Ok(PersistentProviderConfig::default());
    }

    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
}

fn save_config(path: &Path, config: &PersistentProviderConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(config)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PersistentProviderConfig, PersistentProviderProfile};

    #[test]
    fn provider_config_round_trips_profiles() {
        let mut config = PersistentProviderConfig::default();
        config.providers.insert(
            "deepseek".to_string(),
            PersistentProviderProfile {
                api_base: Some("https://api.deepseek.com".to_string()),
                api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
                model_name: Some("deepseek-chat".to_string()),
                system_prompt: None,
            },
        );

        let profile = config.providers.get("deepseek").expect("profile");
        assert_eq!(profile.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
        assert_eq!(profile.model_name.as_deref(), Some("deepseek-chat"));
    }
}
