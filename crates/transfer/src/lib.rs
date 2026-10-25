//! Pure transfer logic for P5 (spec §6, §7): import file readers and file facts. Later tasks add
//! conversion, mapping, batching, dump rendering, restore detection and external-tool arguments.
//!
//! Nothing here talks to a database or spawns a process.

pub mod facts;
pub mod read;

pub use facts::{file_facts, FileFacts};
