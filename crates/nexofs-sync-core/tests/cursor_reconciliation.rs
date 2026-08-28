//! T6-04/SPEC §26.3 ("cursor expirado") — quando o provedor rejeita o
//! cursor de delta persistido (`ProviderErrorKind::CorruptResponse`, o que
//! o Graph real devolve para um cursor expirado/inválido), `SyncCore` deve
//! reconstruir a partir de um cursor novo ("a partir de agora") sem apagar
//! a árvore já indexada — o usuário mantém acesso ao que já tinha durante a
//! reconstrução (T2-03, FR-IDX-006).

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, CreateDirectoryRequest, ProviderAccountContext, ProviderErrorKind};
use nexofs_provider_fake::{FakeProvider, FaultInjectingProvider};
use nexofs_sync_core::{SyncCore, SyncCoreContext};
use std::sync::Arc;

fn account_ctx() -> ProviderAccountContext {
    ProviderAccountContext { account_id: AccountId::new(), provider_account_id: "fake-account".to_string(), tenant_id: None, access_token: SecretToken::new("token") }
}

async fn bootstrap_schema(store: &nexofs_metadata_store::MetadataStore, account_id: AccountId, namespace_id: NamespaceId, remote_namespace_id: &str) {
    let remote_namespace_id = remote_namespace_id.to_string();
    store
        .write(move |tx| {
            tx.execute("INSERT INTO providers (provider_id, display_name, capabilities_json, created_at, updated_at) VALUES ('fake', 'Fake', '{}', 0, 0)", [])?;
            tx.execute(
                "INSERT INTO accounts (account_id, provider_id, provider_account_id, account_type, display_name, auth_state, created_at, updated_at) VALUES (?1, 'fake', 'fake-account', 'PERSONAL', 'Conta Fake', 'VALID', 0, 0)",
                rusqlite::params![account_id.to_string()],
            )?;
            tx.execute(
                "INSERT INTO namespaces (namespace_id, account_id, remote_namespace_id, display_name, namespace_type, mount_path, mount_state, created_at, updated_at) VALUES (?1, ?2, ?3, 'Fake', 'PERSONAL', '/tmp/fake-mount', 'MOUNTED', 0, 0)",
                rusqlite::params![namespace_id.to_string(), account_id.to_string(), remote_namespace_id],
            )
        })
        .await
        .unwrap();
}

async fn cursor_state(store: &nexofs_metadata_store::MetadataStore, namespace_id: NamespaceId) -> String {
    let namespace_id_s = namespace_id.to_string();
    store.read(move |conn| conn.query_row("SELECT cursor_state FROM namespaces WHERE namespace_id = ?1", [namespace_id_s], |row| row.get(0))).await.unwrap()
}

#[tokio::test]
async fn an_expired_cursor_is_reconciled_without_losing_the_indexed_tree() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let fake = Arc::new(FakeProvider::new());
    let faulty = Arc::new(FaultInjectingProvider::new(fake.clone() as Arc<dyn CloudProvider>));
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = fake.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();
    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    bootstrap_schema(&store, account_id, namespace_id, &namespace_remote_id).await;

    // Conteúdo indexado antes de qualquer problema de cursor.
    fake.create_directory(CreateDirectoryRequest {
        account: account_ctx(),
        namespace_remote_id: namespace_remote_id.clone(),
        parent_remote_item_id: None,
        name: "documentos".to_string(),
    })
    .await
    .unwrap();

    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id: namespace_remote_id.clone() };
    let core = SyncCore::new(store.clone(), faulty.clone() as Arc<dyn CloudProvider>, governor, cache, overlay, account_ctx(), ctx);

    let root = core.bootstrap_root().await.unwrap();
    let initial_children = core.list_children(root).await.unwrap();
    assert_eq!(initial_children.len(), 1);
    assert_eq!(initial_children[0].name, "documentos");

    // Primeira chamada: UNINITIALIZED -> pega cursor "a partir de agora".
    core.refresh_changes().await.unwrap();
    assert_eq!(cursor_state(&store, namespace_id).await, "VALID");

    // Simula o cursor persistido virando inválido/expirado no provedor real
    // (Graph devolve algo que `list_changes` classifica como `CorruptResponse`).
    faulty.queue_failure(ProviderErrorKind::CorruptResponse);

    // `refresh_changes` não deve propagar o erro — reconcilia sozinho.
    core.refresh_changes().await.expect("reconciliação deveria absorver o CorruptResponse, não propagá-lo");

    // A árvore já indexada sobrevive intacta.
    let children_after_reconciliation = core.list_children(root).await.unwrap();
    assert_eq!(children_after_reconciliation.len(), 1, "reconciliação de cursor não deveria apagar a árvore já indexada");
    assert_eq!(children_after_reconciliation[0].name, "documentos");

    // E o namespace não fica preso em REBUILDING/ERROR — reconcile_cursor
    // já persiste o cursor novo como VALID antes de retornar.
    assert_eq!(cursor_state(&store, namespace_id).await, "VALID", "cursor deveria voltar a VALID após a reconciliação, não ficar preso em REBUILDING");

    // Uma mudança real depois da reconciliação continua sendo aplicada
    // normalmente pelo novo cursor.
    fake.create_directory(CreateDirectoryRequest {
        account: account_ctx(),
        namespace_remote_id: namespace_remote_id.clone(),
        parent_remote_item_id: None,
        name: "fotos".to_string(),
    })
    .await
    .unwrap();
    core.refresh_changes().await.unwrap();
    let children_final = core.list_children(root).await.unwrap();
    let names: Vec<_> = children_final.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"documentos"));
    assert!(names.contains(&"fotos"), "delta pós-reconciliação deveria continuar funcionando com o cursor novo");
}
