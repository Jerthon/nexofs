//! T3-10/SPEC §26.3 — recuperação de fila após `kill -9` em pleno upload.
//! A garantia equivalente do lado de leitura (download interrompido nunca
//! promovido a conteúdo íntegro) já é coberta por
//! `hydrate_rejects_size_mismatch_and_leaves_no_partial_file` em
//! `nexofs-content-cache`; aqui o foco é a fila de escrita.

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ItemKind, ProviderAccountContext};
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{SyncCore, SyncCoreContext};
use std::path::Path;
use std::sync::Arc;

fn account_ctx() -> ProviderAccountContext {
    ProviderAccountContext {
        account_id: AccountId::new(),
        provider_account_id: "fake-account".to_string(),
        tenant_id: None,
        access_token: SecretToken::new("token"),
    }
}

async fn seed_account_and_namespace(
    store: &nexofs_metadata_store::MetadataStore,
    account_id: AccountId,
    namespace_id: NamespaceId,
    namespace_remote_id: String,
) {
    store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO providers (provider_id, display_name, capabilities_json, created_at, updated_at) VALUES ('fake', 'Fake', '{}', 0, 0)",
                [],
            )?;
            tx.execute(
                "INSERT INTO accounts (account_id, provider_id, provider_account_id, account_type, display_name, auth_state, created_at, updated_at) VALUES (?1, 'fake', 'fake-account', 'PERSONAL', 'Conta Fake', 'VALID', 0, 0)",
                rusqlite::params![account_id.to_string()],
            )?;
            tx.execute(
                "INSERT INTO namespaces (namespace_id, account_id, remote_namespace_id, display_name, namespace_type, mount_path, mount_state, created_at, updated_at) VALUES (?1, ?2, ?3, 'Fake', 'PERSONAL', '/tmp/fake-mount', 'MOUNTED', 0, 0)",
                rusqlite::params![namespace_id.to_string(), account_id.to_string(), namespace_remote_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Reabre um `SyncCore` sobre um diretório já existente — permite simular um
/// reinício do daemon reabrindo o mesmo `nexofs.sqlite3` e o mesmo
/// `cache/dirty/` de uma instância anterior, exatamente como aconteceria
/// depois de um `kill -9` real. `namespaces`/`accounts` já devem existir.
async fn reopen_core_at(
    base: &Path,
    provider: Arc<FakeProvider>,
    account_id: AccountId,
    namespace_id: NamespaceId,
    namespace_remote_id: String,
) -> (Arc<SyncCore>, Arc<nexofs_metadata_store::MetadataStore>) {
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(base.join("nexofs.sqlite3")).unwrap());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(base.join("cache/clean"), base.join("cache/partial"), base.join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(base.join("overlay"));

    let ctx = SyncCoreContext {
        provider_id: ProviderId::from("fake"),
        account_id,
        namespace_id,
        namespace_remote_id,
    };
    let core = Arc::new(SyncCore::new(store.clone(), provider as Arc<dyn CloudProvider>, governor, cache, overlay, account_ctx(), ctx));
    (core, store)
}

#[tokio::test]
async fn an_upload_stuck_running_by_a_kill_9_is_recovered_and_completed_exactly_once_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new());
    let namespaces = provider.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();
    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();

    // Instância 1: cria o arquivo, estabiliza o upload e deixa a operação
    // travada em `RUNNING` — exatamente o estado em que um `kill -9` no meio
    // de `dispatch_upload` deixaria a linha (o processo morre depois de
    // `mark_operation_running`, antes de qualquer conclusão).
    let item_id;
    let operation_id: String;
    {
        let (core1, store1) =
            reopen_core_at(dir.path(), provider.clone(), account_id, namespace_id, namespace_remote_id.clone()).await;
        seed_account_and_namespace(&store1, account_id, namespace_id, namespace_remote_id.clone()).await;
        let root = core1.bootstrap_root().await.unwrap();

        item_id = core1.create_local_item(root, "grande.bin", ItemKind::File).await.unwrap();
        let path = core1.begin_write(item_id).await.unwrap();
        tokio::fs::write(&path, b"conteudo que estava sendo enviado quando o processo morreu").await.unwrap();
        core1.stabilize_upload(item_id).await.unwrap();

        operation_id = store1
            .read({
                let item_id_s = item_id.to_string();
                move |conn| {
                    conn.query_row(
                        "SELECT operation_id FROM operations WHERE item_id = ?1 AND operation_type = 'UPLOAD_FILE'",
                        [item_id_s],
                        |row| row.get(0),
                    )
                }
            })
            .await
            .unwrap();
        core1
            .mark_operation_running(nexofs_domain::OperationId(uuid::Uuid::parse_str(&operation_id).unwrap()))
            .await
            .unwrap();

        // "Processo morto": a instância inteira (incluindo o writer thread
        // do SQLite) sai de escopo sem nunca chamar `dispatch_pending_operations`.
    }

    // Instância 2: "reinício do daemon" — mesmo arquivo de banco, mesmo
    // diretório de cache dirty, mesmo provider (representando que nada foi
    // de fato enviado enquanto a operação morreu presa em `RUNNING`).
    let (core2, store2) =
        reopen_core_at(dir.path(), provider.clone(), account_id, namespace_id, namespace_remote_id.clone()).await;

    let state_before_recovery: String = store2
        .read({
            let operation_id = operation_id.clone();
            move |conn| conn.query_row("SELECT state FROM operations WHERE operation_id = ?1", [operation_id], |row| row.get(0))
        })
        .await
        .unwrap();
    assert_eq!(state_before_recovery, "RUNNING", "pré-condição: a operação deve continuar travada em RUNNING até a recuperação rodar");

    let recovered = core2.recover_running_operations().await.unwrap();
    assert_eq!(recovered, 1);

    core2.dispatch_pending_operations().await.unwrap();

    let item = core2.get_item(item_id).await.unwrap().unwrap();
    assert!(item.remote_item_id.is_some(), "a operação recuperada deve ter sido despachada e completada normalmente");
    assert_eq!(item.sync_state.as_deref(), Some("CLEAN"));

    // Nenhuma duplicata: a chave de idempotência garantiu uma única linha
    // de UPLOAD_FILE para este item, do início ao fim.
    let upload_op_count: i64 = store2
        .read({
            let item_id_s = item_id.to_string();
            move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM operations WHERE item_id = ?1 AND operation_type = 'UPLOAD_FILE'",
                    [item_id_s],
                    |row| row.get(0),
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(upload_op_count, 1, "recuperação não pode duplicar a operação de upload");

    // O conteúdo remoto reflete exatamente o que foi escrito antes da queda.
    let remote_item_id = item.remote_item_id.unwrap();
    let mut downloaded = Vec::new();
    let mut handle = provider
        .open_download(nexofs_provider_api::DownloadRequest {
            account: account_ctx(),
            namespace_remote_id,
            remote_item_id: nexofs_domain::RemoteItemId::from(remote_item_id),
            range: None,
        })
        .await
        .unwrap();
    tokio::io::AsyncReadExt::read_to_end(&mut handle.reader, &mut downloaded).await.unwrap();
    assert_eq!(downloaded, b"conteudo que estava sendo enviado quando o processo morreu");
}
