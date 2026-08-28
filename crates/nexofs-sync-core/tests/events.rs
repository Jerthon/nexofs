//! T5-02/SPEC §20.4 — o barramento de eventos em si já é testado por
//! `EventBus::subscribe`/`publish` implicitamente aqui; o que importa
//! verificar é que o núcleo publica nos pontos certos, com os dados certos.

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ItemKind, ProviderAccountContext};
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{SyncCore, SyncCoreContext, SyncEvent};
use std::sync::Arc;
use std::time::Duration;

fn account_ctx() -> ProviderAccountContext {
    ProviderAccountContext {
        account_id: AccountId::new(),
        provider_account_id: "fake-account".to_string(),
        tenant_id: None,
        access_token: SecretToken::new("token"),
    }
}

async fn build_core() -> (Arc<SyncCore>, nexofs_domain::ItemId, NamespaceId) {
    let dir = tempfile::tempdir().unwrap();
    let dir = Box::leak(Box::new(dir));
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider = Arc::new(FakeProvider::new());
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

    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id };
    let core = Arc::new(SyncCore::new(store, provider.clone() as Arc<dyn CloudProvider>, governor, cache, overlay, account_ctx(), ctx));
    let root = core.bootstrap_root().await.unwrap();
    (core, root, namespace_id)
}

async fn recv_within(rx: &mut tokio::sync::broadcast::Receiver<SyncEvent>, timeout: Duration) -> SyncEvent {
    tokio::time::timeout(timeout, rx.recv()).await.expect("evento esperado não chegou a tempo").unwrap()
}

#[tokio::test]
async fn dispatching_an_operation_publishes_its_progress_through_to_completion() {
    let (core, root, _namespace_id) = build_core().await;
    let mut events = core.subscribe_events();

    core.create_local_item(root, "pasta", ItemKind::Directory).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    // A pasta some direto para RUNNING (nenhuma etapa intermediária) e
    // termina em COMPLETED — como ela é a única operação enfileirada aqui,
    // não há ambiguidade sobre qual evento é qual. `create_local_item`
    // verifica colisão de nome via `lookup_child`, que nesta primeira
    // chamada sobre a raiz (ainda `UNKNOWN`) também dispara um
    // `FolderListed` — ignorado aqui de propósito, não é o que este teste
    // verifica (ver `nexofs-sync-core/tests/lazy_loading.rs`).
    let mut saw_running = false;
    let mut completed_event = None;
    for _ in 0..4 {
        if saw_running && completed_event.is_some() {
            break;
        }
        match recv_within(&mut events, Duration::from_secs(2)).await {
            SyncEvent::OperationProgress { state, .. } if state == "RUNNING" => saw_running = true,
            SyncEvent::OperationProgress { state, operation_type, item_name, item_path, .. } if state == "COMPLETED" => {
                completed_event = Some((operation_type, item_name, item_path));
            }
            SyncEvent::FolderListed { .. } => {}
            other => panic!("evento inesperado: {other:?}"),
        }
    }
    assert!(saw_running, "esperava ver RUNNING para o mkdir despachado");
    // T5-desktop ("mostrar o que está sendo feito"): o evento de conclusão
    // precisa trazer o tipo da operação e o nome/caminho do item — é o que
    // a aba "Log" usa para mostrar "Pasta criada: pasta" em vez de só um
    // `operation_id` opaco.
    let (operation_type, item_name, item_path) = completed_event.expect("esperava ver COMPLETED para o mkdir despachado");
    assert_eq!(operation_type.as_deref(), Some("CREATE_DIRECTORY"));
    assert_eq!(item_name.as_deref(), Some("pasta"));
    assert_eq!(item_path.as_deref(), Some("pasta"));
}

#[tokio::test]
async fn a_refresh_publishes_refresh_completed_for_its_own_namespace() {
    let (core, _root, namespace_id) = build_core().await;
    let mut events = core.subscribe_events();

    core.refresh_changes().await.unwrap();

    match recv_within(&mut events, Duration::from_secs(2)).await {
        SyncEvent::RefreshCompleted { namespace_id: got } => assert_eq!(got, namespace_id),
        other => panic!("esperava RefreshCompleted, recebi {other:?}"),
    }
}
