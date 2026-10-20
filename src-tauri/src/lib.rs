//! DntDatabase app shell: session layer, Tauri commands and logging. The binary
//! (`main.rs`) wires them into the Tauri app.

pub mod cache;
pub mod commands;
pub mod data_sessions;
pub mod desktop;
pub mod export_format;
pub mod export_paths;
pub mod logging;
pub mod query_sessions;
pub mod run_events;
pub mod session_manager;
pub mod state;
