//! Pure transfer logic for P5 (spec §6, §7): import file readers, file facts, cell conversion,
//! column mapping and `INSERT` batching. Later tasks add dump rendering, restore detection and
//! external-tool arguments.
//!
//! Nothing here talks to a database or spawns a process.

pub mod batch;
pub mod convert;
pub mod facts;
pub mod mapping;
pub mod read;

pub use facts::{file_facts, FileFacts};
