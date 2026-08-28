//! API local do daemon — Unix Domain Socket + HTTP/JSON (SPEC §20, ADR-013).
//! UI e CLI nunca acessam SQLite diretamente; só falam com o daemon por aqui.

mod routes;
mod server;
mod state;

pub use server::serve;
pub use state::{AccountControlRequest, AccountSummary, AddAccountRequest, AppState, NamespaceSummary};
