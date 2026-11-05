//! MongoDB driver built on the official `mongodb` crate (rustls with ring only).

mod client;
pub mod rows;
pub mod shell;

use async_trait::async_trait;
use dnt_core::command::CommandLanguage;
use dnt_core::config::ResolvedConfig;
use dnt_core::driver::{Driver, Session};
use dnt_core::error::DbError;
use dnt_core::server::DbKind;
use std::sync::Arc;

/// The connect step without a session, for integration tests.
#[cfg(feature = "test-helpers")]
pub mod test_helpers {
    pub use crate::client::{open, Opened};
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MongoDriver;

#[async_trait]
impl Driver for MongoDriver {
    fn kind(&self) -> DbKind {
        DbKind::Mongo
    }

    fn command_language(&self) -> Option<Arc<dyn CommandLanguage>> {
        Some(Arc::new(shell::MongoLanguage))
    }

    /// Connects and checks the server; the session itself comes with `MongoSession`.
    async fn connect(&self, cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        let opened = client::open(cfg).await?;
        opened.client.shutdown().immediate(true).await;
        Err(DbError::Unsupported {
            feature: "MongoDB sessions".into(),
            version: String::new(),
        })
    }
}
