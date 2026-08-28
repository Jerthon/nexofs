//! Regressão: `cache_stats` chegou a agregar `local_states` do banco
//! inteiro, sem filtrar por `namespace_id` — toda conta hidratada aparecia
//! com o mesmo total (bug real, achado pelo usuário testando a aba "Cache"
//! do app com múltiplas contas mostrando sempre o mesmo tamanho).

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ProviderAccountContext, UploadRequest};
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

/// Cria um namespace novo (linha no banco compartilhado + `SyncCore` próprio)
/// com um único arquivo já hidratado de `size_bytes` bytes.
async fn namespace_with_hydrated_file(
    store: &Arc<nexofs_metadata_store::MetadataStore>,
    provider: &Arc<FakeProvider>,
    governor: &Arc<ProviderApiGovernor>,
    remote_namespace_id: &str,
    content: &'static [u8],
) -> Arc<SyncCore> {
    let provider_dyn: Arc<dyn CloudProvider> = provider.clone();
    provider_dyn
        .upload(UploadRequest {
            account: account_ctx(),
            namespace_remote_id: remote_namespace_id.to_string(),
            parent_remote_item_id: None,
            name: "arquivo.bin".to_string(),
            size_bytes: content.len() as u64,
            base_remote_version: None,
            content: Box::pin(content),
            resumable_session_token: None,
        })
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dir = Box::leak(Box::new(dir));
    let cache = ContentCache::new(dir.path().join("clean"), dir.path().join("partial"), dir.path().join("dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    {
        let account_id_s = account_id.to_string();
        let namespace_id_s = namespace_id.to_string();
        let remote_namespace_id = remote_namespace_id.to_string();
        let mount_path = format!("/tmp/fake-mount-{namespace_id_s}");
        store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO accounts (account_id, provider_id, provider_account_id, account_type, display_name, auth_state, created_at, updated_at) VALUES (?1, 'fake', ?1, 'PERSONAL', 'Conta Fake', 'VALID', 0, 0)",
                    rusqlite::params![account_id_s],
                )?;
                tx.execute(
                    "INSERT INTO namespaces (namespace_id, account_id, remote_namespace_id, display_name, namespace_type, mount_path, mount_state, created_at, updated_at) VALUES (?1, ?2, ?3, 'Fake', 'PERSONAL', ?4, 'MOUNTED', 0, 0)",
                    rusqlite::params![namespace_id_s, account_id_s, remote_namespace_id, mount_path],
                )
            })
            .await
            .unwrap();
    }

    let ctx = SyncCoreContext {
        provider_id: ProviderId::from("fake"),
        account_id,
        namespace_id,
        namespace_remote_id: remote_namespace_id.to_string(),
    };
    let core = Arc::new(SyncCore::new(store.clone(), provider_dyn, governor.clone(), cache, overlay, account_ctx(), ctx));

    let root = core.bootstrap_root().await.unwrap();
    let children = core.list_children(root).await.unwrap();
    let file = children.first().expect("upload deveria ter criado um arquivo");
    core.open_and_hydrate(file.item_id).await.unwrap();

    core
}

#[tokio::test]
async fn cache_stats_never_leaks_bytes_between_namespaces_sharing_the_same_store() {
    let store_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(store_dir.path().join("nexofs.sqlite3")).unwrap());
    store
        .write(|tx| tx.execute("INSERT INTO providers (provider_id, display_name, capabilities_json, created_at, updated_at) VALUES ('fake', 'Fake', '{}', 0, 0)", []))
        .await
        .unwrap();

    let provider = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());

    // Duas contas/namespaces distintos no MESMO banco — exatamente o caso
    // real (várias contas OneDrive indexadas na mesma SQLite do daemon).
    // Tamanhos bem diferentes de propósito: se `cache_stats` voltar a
    // ignorar `namespace_id`, os dois totais colidem no mesmo valor (a
    // soma das duas), o que este teste pega imediatamente.
    let core_a = namespace_with_hydrated_file(&store, &provider, &governor, "ns-a", &[0u8; 10]).await;
    let core_b = namespace_with_hydrated_file(&store, &provider, &governor, "ns-b", &[0u8; 4000]).await;

    let stats_a = core_a.cache_stats().await.unwrap();
    let stats_b = core_b.cache_stats().await.unwrap();

    assert_eq!(stats_a.hydrated_items, 1);
    assert_eq!(stats_a.hydrated_bytes, 10);
    assert_eq!(stats_b.hydrated_items, 1);
    assert_eq!(stats_b.hydrated_bytes, 4000);

    // T5-07: `cache_breakdown` é uma segunda query sobre a mesma tabela —
    // precisa da mesma isolação por namespace que `cache_stats`, não só a
    // categorização em si.
    let breakdown_a = core_a.cache_breakdown().await.unwrap();
    let breakdown_b = core_b.cache_breakdown().await.unwrap();
    assert_eq!(breakdown_a.clean_items, 1);
    assert_eq!(breakdown_a.clean_bytes, 10);
    assert_eq!(breakdown_b.clean_items, 1);
    assert_eq!(breakdown_b.clean_bytes, 4000);
    assert_eq!(breakdown_a.dirty_bytes, 0);
    assert_eq!(breakdown_a.overlay_bytes, 0);
}
