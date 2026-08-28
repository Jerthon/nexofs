//! PRAGMAs obrigatórios em toda conexão. SPEC §10.2.

use rusqlite::Connection;

pub fn apply(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "wal_autocheckpoint", 2000)?;
    Ok(())
}
