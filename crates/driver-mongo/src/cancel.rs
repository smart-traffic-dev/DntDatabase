//! Cancel (spec §5.5): the running command is found by its comment (`dnt:<uuid>`) in
//! `$currentOp` and stopped with `killOp`. The backend raises `opts.stop` and calls
//! [`MongoCanceller::cancel`]; `execute_command` waits out the grace period (`session.rs`).

use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use dnt_core::command::CommandClass;
use dnt_core::driver::Canceller;
use dnt_core::error::DbError;
use mongodb::bson::{doc, Bson};
use mongodb::Client;

use crate::exec::map_error;
use crate::session::Running;

/// Kills the session's running command. Holds no `Client` of its own: it reads the
/// session's current one, so a handle taken before a `Client` rebuild still works after it.
pub(crate) struct MongoCanceller {
    pub client: Arc<RwLock<Client>>,
    pub running: Arc<Mutex<Option<Running>>>,
}

#[async_trait]
impl Canceller for MongoCanceller {
    /// Nothing runs (the cancel fell between two commands): nothing to do.
    async fn cancel(&self) -> Result<(), DbError> {
        let comment = match &*self.running.lock().unwrap_or_else(|p| p.into_inner()) {
            Some(r) => comment_of(&r.uuid),
            None => return Ok(()),
        };
        let client = self
            .client
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        kill(&client, &comment).await
    }
}

/// The comment every command of a run carries.
pub(crate) fn comment_of(uuid: &str) -> String {
    format!("dnt:{uuid}")
}

/// `killOp` of every op of this user whose command carries `comment` (a `getMore` carries
/// its command's comment, so a cursor being read is found too). None found: nothing to do.
pub(crate) async fn kill(client: &Client, comment: &str) -> Result<(), DbError> {
    let failed = |e: mongodb::error::Error| map_error(&e, &CommandClass::Read).0;
    let admin = client.database("admin");
    let mut ops = admin
        .aggregate([
            doc! { "$currentOp": { "allUsers": false } },
            doc! { "$match": { "command.comment": comment } },
        ])
        .await
        .map_err(failed)?;
    let mut opids: Vec<Bson> = Vec::new();
    while ops.advance().await.map_err(failed)? {
        let op = ops.deserialize_current().map_err(failed)?;
        if let Some(id) = op.get("opid") {
            opids.push(id.clone());
        }
    }
    for op in opids {
        admin
            .run_command(doc! { "killOp": 1, "op": op })
            .await
            .map_err(failed)?;
    }
    Ok(())
}
