use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use tokio::fs;
use zetta_protocol::{SessionId, SessionSnapshot};

use super::SessionStore;

pub struct FileSessionStore {
    root: PathBuf,
}

impl FileSessionStore {
    #[must_use]
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn session_path(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(format!("{session_id}.json"))
    }

    pub async fn delete(&self, session_id: &SessionId) -> Result<()> {
        let path = self.session_path(session_id);
        match fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn load(&self, session_id: &SessionId) -> Result<Option<SessionSnapshot>> {
        let path = self.session_path(session_id);
        match fs::read_to_string(path).await {
            Ok(contents) => Ok(Some(serde_json::from_str(&contents)?)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn save(&self, session: &SessionSnapshot) -> Result<()> {
        fs::create_dir_all(&self.root).await?;
        let path = self.session_path(&session.session_id);
        fs::write(path, serde_json::to_vec_pretty(session)?).await?;
        Ok(())
    }
}
