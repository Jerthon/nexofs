use nexofs_api_client::ApiClient;
use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_local_api::AppState;
use nexofs_provider_api::{CloudProvider, ProviderAccountContext};
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{EventBus, SyncCore, SyncCoreContext};
use std::collections::HashMap;
use std::sync::Arc;

fn account_ctx() -> ProviderAccountContext {
    ProviderAccountContext { account_id: AccountId::new(), provider_account_id: "fake-account".to_string(), tenant_id: None, access_token: SecretToken::new("token") }
}

async fn spawn_test_server() -> (std::path::PathBuf, Arc<SyncCore>) {
    let store_dir = tempfile::tempdir().unwrap();
    let store_dir = Box::leak(Box::new(store_dir));
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(store_dir.path().join("nexofs.sqlite3")).unwrap());
    let governor = Arc::new(ProviderApiGovernor::new());
    let provider = Arc::new(FakeProvider::new());
    let dir = tempfile::tempdir().unwrap();
    let dir = Box::leak(Box::new(dir));
    let cache = ContentCache::new(dir.path().join("clean"), dir.path().join("partial"), dir.path().join("dirty"));
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

    let event_bus = Arc::new(EventBus::new());
    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id };
    let core = Arc::new(SyncCore::new(store, provider.clone() as Arc<dyn CloudProvider>, governor.clone(), cache, overlay, account_ctx(), ctx).with_event_bus(event_bus.clone()));
    core.bootstrap_root().await.unwrap();

    let mut cores = HashMap::new();
    cores.insert(namespace_id, core.clone());
    let state = AppState::new(cores, Vec::new(), Vec::new(), governor, event_bus, 1024, std::env::temp_dir());

    let socket_dir = tempfile::tempdir().unwrap();
    let socket_path = socket_dir.path().join("control.sock");
    let path_for_task = socket_path.clone();
    std::mem::forget(socket_dir);
    tokio::spawn(async move {
        let _ = nexofs_local_api::serve(&path_for_task, state).await;
    });
    for _ in 0..50 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    (socket_path, core)
}

#[tokio::test]
async fn get_and_post_round_trip_through_the_real_unix_socket_server() {
    let (socket_path, _core) = spawn_test_server().await;
    let client = ApiClient::new(socket_path);

    let status = client.get("/v1/status").await.unwrap();
    assert!(status["namespaces"].is_array());

    let err = client.get("/v1/namespaces/00000000-0000-0000-0000-000000000000/refresh").await;
    assert!(err.is_err(), "método errado num caminho de rota inexistente deve virar Err, não um valor mudo");
}

#[tokio::test]
async fn a_non_2xx_response_becomes_an_error_with_the_daemons_message() {
    let (socket_path, _core) = spawn_test_server().await;
    let client = ApiClient::new(socket_path);

    let result = client.post("/v1/namespaces/00000000-0000-0000-0000-000000000000/refresh", None).await;
    let err = result.expect_err("namespace inexistente deve responder 404, que o cliente deve transformar em Err");
    assert!(err.to_string().contains("404"));
}

#[tokio::test]
async fn stream_events_calls_on_connected_once_before_any_line_then_delivers_events() {
    let (socket_path, core) = spawn_test_server().await;
    let client = ApiClient::new(socket_path);

    let (connected_tx, mut connected_rx) = tokio::sync::mpsc::unbounded_channel();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _ = client
            .stream_events(
                move || {
                    let _ = connected_tx.send(());
                },
                move |line| {
                    let _ = tx.send(line.to_string());
                },
            )
            .await;
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), connected_rx.recv())
        .await
        .expect("on_connected deve disparar assim que o cabeçalho HTTP chega, antes de qualquer evento")
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    core.refresh_changes().await.unwrap();

    let line = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert!(line.contains("REFRESH_COMPLETED"), "linha recebida: {line}");
}
