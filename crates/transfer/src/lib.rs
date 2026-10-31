//! Pure transfer logic for P5 (spec §6, §7): import file readers, file facts, cell conversion,
//! column mapping, `INSERT` batching and dump rendering. Later tasks add restore detection and
//! external-tool arguments.
//!
//! Nothing here talks to a database or spawns a process.

pub mod batch;
pub mod convert;
pub mod detect;
pub mod dump;
pub mod facts;
pub mod mapping;
pub mod read;
pub mod seed;
pub mod tools;

pub use facts::{file_facts, FileFacts};
