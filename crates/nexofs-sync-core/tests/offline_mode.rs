//! T3-09/FR-OFF-005 — detecção de conectividade e retomada após reconexão.
//! Usa `FaultInjectingProvider` (nexofs-provider-fake) para simular queda de
//! rede sem depender de infraestrutura real.

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ItemKind, ProviderAccountContext};
use nexofs_provider_fake::{FakeProvider, FaultInjectingProvider};
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

async fn build_core() -> (Arc<SyncCore>, nexofs_domain::ItemId, Arc<nexofs_metadata_store::MetadataStore>, Arc<FaultInjectingProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let dir = Box::leak(Box::new(dir));
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let fake = Arc::new(FakeProvider::new());
    let provider = Arc::new(FaultInjectingProvider::new(fake.clone() as Arc<dyn CloudProvider>));
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = fake.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();

    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    {
        let namespace_remote_id_for_row = namespace_remote_id.clone();
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
                    rusqlite::params![namespace_id.to_string(), account_id.to_string(), namespace_remote_id_for_row],
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
    let core = Arc::new(SyncCore::new(store.clone(), provider.clone() as Arc<dyn CloudProvider>, governor, cache, overlay, account_ctx(), ctx));
    let root = core.bootstrap_root().await.unwrap();
    (core, root, store, provider)
}

#[tokio::test]
async fn starts_online_by_default() {
    let (core, ..) = build_core().await;
    assert!(core.is_online().await, "sem nenhuma chamada ainda, o padrão otimista deve ser online");
}

#[tokio::test]
async fn a_network_failure_during_dispatch_marks_the_operation_waiting_network_and_flips_offline() {
    let (core, root, store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "arquivo.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();

    provider.queue_network_failures(1);
    core.dispatch_pending_operations().await.unwrap();

    assert!(!core.is_online().await, "uma falha de rede real deve derrubar o sinal de conectividade");
    let state: String = store
        .read(move |conn| {
            conn.query_row(
                "SELECT state FROM operations WHERE item_id = ?1 AND operation_type = 'UPLOAD_FILE'",
                [item_id.to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(state, "WAITING_NETWORK", "falha de Network/Timeout deve virar WAITING_NETWORK, não o WAITING_RETRY genérico");
}

#[tokio::test]
async fn reconnecting_immediately_wakes_a_waiting_network_operation_instead_of_waiting_out_the_backoff() {
    let (core, root, _store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "arquivo.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();

    provider.queue_network_failures(1);
    core.dispatch_pending_operations().await.unwrap();
    assert!(!core.is_online().await, "pré-condição: offline após a falha injetada");

    // `next_attempt_at` ficou no futuro (rede de segurança de 30s) — sem a
    // detecção de reconexão, uma segunda rodada imediata do dispatcher não
    // pegaria nada. Confirma isso antes de provar que a reconexão muda o
    // resultado.
    core.dispatch_pending_operations().await.unwrap();
    let item_after_immediate_retry = core.get_item(item_id).await.unwrap().unwrap();
    assert!(item_after_immediate_retry.remote_item_id.is_none(), "pré-condição: backoff ainda não venceu, nada deveria ter sido despachado de novo");

    // Qualquer chamada governada bem-sucedida detecta a reconexão — aqui,
    // uma chamada de leitura completamente não relacionada ao upload pendente.
    core.refresh_changes().await.unwrap();
    assert!(core.is_online().await, "uma chamada bem-sucedida deve restaurar o sinal de conectividade");

    // A reconexão já deve ter liberado o upload para o próximo tick, sem
    // esperar os 30s do backoff de segurança.
    core.dispatch_pending_operations().await.unwrap();
    let item_after_reconnect = core.get_item(item_id).await.unwrap().unwrap();
    assert!(item_after_reconnect.remote_item_id.is_some(), "reconexão detectada deve liberar a operação WAITING_NETWORK imediatamente");
}
