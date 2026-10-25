//! DntDatabase app shell: session layer, Tauri commands and logging. The binary
//! (`main.rs`) wires them into the Tauri app.

pub mod cache;
pub mod commands;
pub mod data_sessions;
pub mod ddl_exec;
pub mod desktop;
pub mod edit_plans;
pub mod export_format;
pub mod export_paths;
pub mod file_picks;
pub mod logging;
pub mod query_sessions;
pub mod result_store;
pub mod run_events;
pub mod session_manager;
pub mod state;

pub mod object_actions;
pub mod table_design;

pub mod import_data;
pub mod jobs;

pub mod backup;
