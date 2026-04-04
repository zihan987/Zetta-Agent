mod file_store;

use anyhow::Result;
use async_trait::async_trait;
use zetta_protocol::{SessionId, SessionSnapshot};

pub use file_store::FileSessionStore;

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn load(&self, session_id: &SessionId) -> Result<Option<SessionSnapshot>>;
    async fn save(&self, session: &SessionSnapshot) -> Result<()>;
}
