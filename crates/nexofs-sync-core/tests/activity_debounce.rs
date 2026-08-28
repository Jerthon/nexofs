//! Regressão: `nexofs-fuse` chama `mark_directory_active` de uma thread
//! nativa do fuser, fora de qualquer runtime Tokio. Uma versão anterior
//! usava `tokio::spawn` (a função livre, que exige estar dentro de um
//! runtime já ativo) internamente e entrava em pânico nesse cenário,
//! derrubando a sessão FUSE inteira. Ver `SyncCore::runtime_handle`.

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ProviderAccountContext};
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{SyncCore, SyncCoreContext};
use std::sync::Arc;

fn account_ctx() -> ProviderAccountContext {
    ProviderAccountContext {
        account_id: AccountId::new(),
        provider_account_id: "fake-account".to_string(),
        tenant_id: None,
        access_token: SecretToken::new("token"),
    }
}

#[tokio::test]
async fn mark_directory_active_does_not_panic_when_called_from_a_thread_without_a_tokio_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = provider.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();
    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();

    {
        let remote_namespace_id = namespace_remote_id.clone();
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
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    let ctx = SyncCoreContext {
        provider_id: ProviderId::from("fake"),
        account_id,
        namespace_id,
        namespace_remote_id,
    };
    // `SyncCore::new` captura o `Handle` do runtime atual (o do
    // `#[tokio::test]`) — é isso que permite o `spawn` funcionar mesmo
    // quando chamado de fora dele, a seguir.
    let core = Arc::new(SyncCore::new(store, provider, governor, cache, overlay, account_ctx(), ctx));
    let root = core.bootstrap_root().await.unwrap();

    // Simula exatamente o que `nexofs-fuse` faz: chama de uma `std::thread`
    // pura, sem nenhum runtime Tokio associado a ela.
    let core_for_thread = core.clone();
    let handle = std::thread::spawn(move || {
        core_for_thread.mark_directory_active(root, true);
    });

    handle.join().expect("a thread não deveria entrar em pânico");

    // Dá tempo da task de refresh (spawada via `runtime_handle`) rodar.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(core.active_directory_count(), 1);
}
