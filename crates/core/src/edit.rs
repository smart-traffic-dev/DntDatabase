//! The data-view edits the UI stages (spec §4.2a, §7.4): pure data, shared by the SQL
//! planner (`dnt_query::edit`) and the Command drivers' [`crate::data_access::DataAccess`].

use crate::meta::ObjectRef;
use crate::value::Value;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What the user typed or picked for one cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CellInput {
    Null,
    /// The column's default (new rows only).
    Default,
    /// Text typed in the grid, parsed by the column's logical type.
    Text {
        text: String,
    },
    Value {
        value: Value,
    },
    /// Binary data (standard base64). Renamed explicitly: specta's snake_case would
    /// export `base_64` while serde reads and writes `base64`.
    #[serde(rename = "base64")]
    Base64 {
        data: String,
    },
    /// A full value fetched earlier, by its token.
    Loaded {
        token: Uuid,
    },
}

/// One loaded row: its key values, every loaded cell as it was, and (PostgreSQL) its
/// `xmin::text`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct RowRef {
    pub key: Vec<(String, Value)>,
    pub original: Vec<(String, Value)>,
    pub xmin: Option<String>,
    /// Cells of `original` the page may have cut that were fetched in full
    /// (`data_cell_fetch`): column → token. Such a cell joins the row guard's snapshot
    /// with its complete value; a cut cell without one stays unchecked.
    #[serde(default)]
    pub loaded: Vec<(String, Uuid)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RowEdit {
    Update {
        row: RowRef,
        set: Vec<(String, CellInput)>,
    },
    Insert {
        values: Vec<(String, CellInput)>,
    },
    Delete {
        row: RowRef,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct EditSet {
    pub object: ObjectRef,
    pub edits: Vec<RowEdit>,
    /// Drop every row guard (an explicit "Overwrite" after a conflict).
    pub overwrite: bool,
}
