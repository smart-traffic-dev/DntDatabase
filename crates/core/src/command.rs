//! The contract of a Command-family engine (MongoDB, Redis): its language (split, parse,
//! redact) and the session that runs parsed commands (spec §4.1).

use crate::data_access::DataAccess;
use crate::driver::{ExecOptions, RowSink, Session, StmtOutcome};
use crate::error::DbError;
use crate::redact::Redacted;
use crate::sql::TxControl;
use crate::tx::TxState;
use async_trait::async_trait;
use std::any::Any;
use std::fmt;
use std::sync::Arc;

/// The command language of one Command engine.
pub trait CommandLanguage: Send + Sync {
    /// The commands of a script, in order; `range` is in bytes of `text`. Never panics.
    fn split(&self, text: &str) -> Vec<CommandSpan>;
    /// One command. A syntax error is an `Err`, never a command let through as unknown.
    fn parse(&self, text: &str) -> Result<ParsedCommand, CommandParseError>;
    /// The form of `text` that is safe to keep in History, logs and Favorites.
    fn redact(&self, text: &str) -> Redacted;
}

/// One command of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpan {
    /// Byte range of the command in the script.
    pub range: (usize, usize),
    pub text: String,
}

/// The driver's opaque parsed form of a command. Core never reads it;
/// [`CommandSession::execute_command`] downcasts it to the driver's own type, and a
/// payload of another type is `DbError::Internal`.
pub type CommandPayload = Arc<dyn Any + Send + Sync>;

/// One parsed command: what the guard needs to know, plus the driver's payload.
#[derive(Clone)]
pub struct ParsedCommand {
    /// The command as the engine names it: `find`, `updateMany`, `HSET`.
    pub name: String,
    pub class: CommandClass,
    /// The collections / keys / patterns it acts on.
    pub targets: Vec<CommandTarget>,
    /// It acts on every document / key of its target (like an `UPDATE` without `WHERE`).
    pub unbounded: bool,
    /// What it drops: a collection, a database, `FLUSHDB` / `FLUSHALL`.
    pub drops: Vec<CommandDrop>,
    /// How it changes the session (Redis `SELECT n`, Mongo `use db`).
    pub session_effect: Option<CommandSessionEffect>,
    pub payload: CommandPayload,
}

impl fmt::Debug for ParsedCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedCommand")
            .field("name", &self.name)
            .field("class", &self.class)
            .field("targets", &self.targets)
            .field("unbounded", &self.unbounded)
            .field("drops", &self.drops)
            .field("session_effect", &self.session_effect)
            .field("payload", &format_args!("<payload>"))
            .finish()
    }
}

/// What a command does, for the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandClass {
    Read,
    Write,
    Ddl,
    Admin,
    SessionChange,
    Transaction(TxControl),
    /// Not in the driver's command table.
    Unknown,
}

/// A collection, key or key pattern; `database` when the command names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTarget {
    /// `None`: the database in effect when this command runs (the session's, as
    /// changed by the script's earlier `SessionChange` commands), not the one current
    /// when the command was parsed. A language never fills it from session state.
    pub database: Option<String>,
    pub name: String,
}

/// Something a command drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDrop {
    pub kind: CommandDropKind,
    /// For a [`CommandDropKind::Database`] drop, `target.name` is the database; an
    /// empty name means the database in effect when this command runs (`FLUSHDB`), as
    /// for a [`CommandTarget`] with `database: None`.
    pub target: CommandTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandDropKind {
    /// A collection (like `DROP TABLE`).
    Object,
    /// A database: `dropDatabase`, `FLUSHDB`, `FLUSHALL` (like `DROP DATABASE`).
    Database,
}

/// How a command changes its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSessionEffect {
    UseDatabase(String),
}

/// Why a command could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandParseError {
    pub message: String,
    /// Byte offset of the error in the command, when known.
    pub offset: Option<usize>,
}

/// A session that runs commands.
#[async_trait]
pub trait CommandSession: Session {
    fn language(&self) -> &dyn CommandLanguage;
    fn tx_state(&self) -> TxState;
    fn data_access(&self) -> &dyn DataAccess;
    /// Runs ONE parsed command, with the same contract as
    /// [`crate::driver::SqlSession::execute_one`] for the sink, stop, timeout, cancel
    /// grace and `was_in_tx`. `opts.params` is ignored (SQL only).
    async fn execute_command(
        &self,
        cmd: &ParsedCommand,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsed_command_debug_hides_the_payload() {
        let cmd = ParsedCommand {
            name: "GET".into(),
            class: CommandClass::Read,
            targets: vec![CommandTarget {
                database: None,
                name: "k".into(),
            }],
            unbounded: false,
            drops: vec![],
            session_effect: None,
            payload: Arc::new(String::from("secret")),
        };
        let text = format!("{:?}", cmd.clone());
        assert!(text.contains("payload: <payload>"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }
}
