//! PostgreSQL rendering: placeholder until Task 8 fills it in.

use super::{RenderError, Rendered};
use crate::diff::Change;
use crate::model::*;
use dnt_core::server::ServerInfo;
use dnt_core::sql::build::TableRef;
use dnt_core::sql::Dialect;

fn not_implemented<T>() -> Result<T, RenderError> {
    Err(RenderError::Invalid {
        message: "not implemented".into(),
    })
}

pub(super) fn render(
    _table: &TableRef,
    _old: &TableDef,
    _changes: &[Change],
    _server: &ServerInfo,
    _dialect: Dialect,
) -> Result<Rendered, RenderError> {
    not_implemented()
}

pub(super) fn render_create(
    _table: &TableRef,
    _def: &TableDef,
    _server: &ServerInfo,
    _dialect: Dialect,
) -> Result<Rendered, RenderError> {
    not_implemented()
}

pub(super) fn column_sql(
    _c: &ColumnDef,
    _server: &ServerInfo,
    _dialect: Dialect,
) -> Result<String, RenderError> {
    not_implemented()
}

pub(super) fn primary_key_sql(
    _k: &KeyDef,
    _server: &ServerInfo,
    _dialect: Dialect,
) -> Result<String, RenderError> {
    not_implemented()
}

pub(super) fn index_sql(
    _i: &IndexDef,
    _server: &ServerInfo,
    _dialect: Dialect,
) -> Result<String, RenderError> {
    not_implemented()
}

pub(super) fn fk_sql(
    _f: &FkDef,
    _server: &ServerInfo,
    _dialect: Dialect,
) -> Result<String, RenderError> {
    not_implemented()
}

pub(super) fn check_sql(
    _c: &CheckDef,
    _server: &ServerInfo,
    _dialect: Dialect,
) -> Result<String, RenderError> {
    not_implemented()
}
