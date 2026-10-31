//! How a Command driver builds the commands of the data view and its edits (spec §7.4).

/// The data-view face of a Command session. Its methods come with the data view.
pub trait DataAccess: Send + Sync {}
