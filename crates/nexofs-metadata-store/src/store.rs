//! Escritor único serializado + leitores sob demanda. SPEC §10.4.
//!
//! Todas as escritas passam por uma única thread dedicada, evitando
//! `SQLITE_BUSY` por contenção de escritor e garantindo que não há dois
//! commits concorrentes disputando o mesmo arquivo WAL. Leituras abrem uma
//! conexão própria por chamada — WAL permite múltiplos leitores concorrentes
//! sem bloquear o escritor; um pool de conexões reutilizáveis fica para
//! quando um benchmark (Fase 6) mostrar que o custo de abertura pesa.

use crate::{migrations, pragmas};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("erro de banco de dados: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("a thread de escrita do metadata store encerrou inesperadamente")]
    WriterGone,
}

pub struct MetadataStore {
    db_path: PathBuf,
    write_tx: std_mpsc::Sender<WriteJob>,
    // Mantém a thread viva pelo tempo de vida do store; join só ocorre no Drop.
    _writer_thread: std::thread::JoinHandle<()>,
}

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    pragmas::apply(&conn)?;
    Ok(conn)
}

impl MetadataStore {
    /// Abre (ou cria) o banco em `db_path`, aplica PRAGMAs e migrations, e
    /// inicia a thread de escrita. Bloqueia até a thread confirmar prontidão.
    pub fn open(db_path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let db_path = db_path.into();
        let (write_tx, write_rx) = std_mpsc::channel::<WriteJob>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), rusqlite::Error>>();

        let thread_path = db_path.clone();
        let writer_thread = std::thread::Builder::new()
            .name("nexofs-metadata-writer".to_string())
            .spawn(move || {
                let mut conn = match open_connection(&thread_path).and_then(|mut c| {
                    migrations::run(&mut c)?;
                    Ok(c)
                }) {
                    Ok(conn) => {
                        let _ = ready_tx.send(Ok(()));
                        conn
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };

                while let Ok(job) = write_rx.recv() {
                    job(&mut conn);
                }
            })
            .expect("falha ao criar a thread de escrita do metadata store");

        ready_rx
            .recv()
            .map_err(|_| StoreError::WriterGone)??;

        Ok(Self {
            db_path,
            write_tx,
            _writer_thread: writer_thread,
        })
    }

    /// Executa `f` dentro de uma transação na thread de escrita única e
    /// aguarda o resultado. `f` roda em contexto síncrono (bloqueante) — não
    /// faça I/O de rede dentro dela.
    pub async fn write<F, T>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let job: WriteJob = Box::new(move |conn| {
            let outcome: rusqlite::Result<T> = (|| {
                let tx = conn.transaction()?;
                let value = f(&tx)?;
                tx.commit()?;
                Ok(value)
            })();
            let _ = resp_tx.send(outcome);
        });

        self.write_tx.send(job).map_err(|_| StoreError::WriterGone)?;
        resp_rx.await.map_err(|_| StoreError::WriterGone)?.map_err(StoreError::from)
    }

    /// Abre uma conexão de leitura isolada e executa `f` em uma thread
    /// bloqueante dedicada, sem competir pelo escritor único.
    pub async fn read<F, T>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_connection(&db_path)?;
            f(&conn)
        })
        .await
        .map_err(|_| StoreError::WriterGone)?
        .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writer_serializes_and_reader_sees_committed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap();

        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO providers (provider_id, display_name, capabilities_json, created_at, updated_at) VALUES ('fake', 'Fake', '{}', 0, 0)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let count = store
            .read(|conn| conn.query_row("SELECT COUNT(*) FROM providers", [], |row| row.get::<_, i64>(0)))
            .await
            .unwrap();

        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn concurrent_reads_do_not_block_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());

        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store
                    .read(|conn| conn.query_row("SELECT COUNT(*) FROM providers", [], |row| row.get::<_, i64>(0)))
                    .await
                    .unwrap()
            }));
        }

        for handle in handles {
            assert_eq!(handle.await.unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn concurrent_writes_are_serialized_without_sqlite_busy() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());

        let mut handles = Vec::new();
        for i in 0..20 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store
                    .write(move |tx| {
                        tx.execute(
                            "INSERT INTO providers (provider_id, display_name, capabilities_json, created_at, updated_at) VALUES (?1, 'Fake', '{}', 0, 0)",
                            [format!("fake-{i}")],
                        )
                    })
                    .await
                    .unwrap()
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let count = store
            .read(|conn| conn.query_row("SELECT COUNT(*) FROM providers", [], |row| row.get::<_, i64>(0)))
            .await
            .unwrap();
        assert_eq!(count, 20);
    }
}
