use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Function,
    Procedure,
    Trigger,
    Event,
    Sequence,
    Type,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct ObjectRef {
    pub database: String,
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
}
