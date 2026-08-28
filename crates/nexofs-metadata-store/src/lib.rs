//! Índice local persistente: SQLite em WAL, migrations e o escritor único
//! serializado exigido pela SPEC §10.4.

mod migrations;
mod pragmas;
mod store;

pub use rusqlite;
pub use store::{MetadataStore, StoreError};
