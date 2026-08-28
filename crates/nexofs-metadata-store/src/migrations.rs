//! Runner de migrations transacional. SPEC §10.5.
//!
//! A versão aplicada é rastreada via `PRAGMA user_version` — dispensa uma
//! tabela própria de controle para o schema inicial. Cada entrada da lista
//! é um script idempotente aplicado uma única vez, em ordem, dentro de uma
//! única transação.

use rusqlite::Connection;

struct Migration {
    version: i64,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("../../../migrations/0001_init.sql"),
}];

pub fn run(conn: &mut Connection) -> rusqlite::Result<()> {
    let current_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    for migration in MIGRATIONS.iter().filter(|m| m.version > current_version) {
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.pragma_update(None, "user_version", migration.version)?;
        tx.commit()?;
        tracing::info!(version = migration.version, "migration aplicada");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_once_and_is_idempotent_on_reopen() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();

        // Confirma que o schema existe.
        conn.execute("INSERT INTO providers (provider_id, display_name, capabilities_json, created_at, updated_at) VALUES ('fake', 'Fake', '{}', 0, 0)", []).unwrap();

        // Rodar de novo não deve tentar recriar as tabelas.
        run(&mut conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM providers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
