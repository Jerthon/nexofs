use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_local_api::AppState;
use nexofs_provider_api::{CloudProvider, ProviderAccountContext};
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{SyncCore, SyncCoreContext};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn account_ctx() -> ProviderAccountContext {
    ProviderAccountContext {
        account_id: AccountId::new(),
        provider_account_id: "fake-account".to_string(),
        tenant_id: None,
        access_token: SecretToken::new("token"),
    }
}

async fn build_sync_core(store: Arc<nexofs_metadata_store::MetadataStore>, governor: Arc<ProviderApiGovernor>) -> (NamespaceId, Arc<SyncCore>) {
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    let dir = tempfile::tempdir().unwrap();
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

    let ctx = SyncCoreContext {
        provider_id: ProviderId::from("fake"),
        account_id,
        namespace_id,
        namespace_remote_id,
    };
    let core = Arc::new(SyncCore::new(store, provider, governor, cache, overlay, account_ctx(), ctx));
    core.bootstrap_root().await.unwrap();
    (namespace_id, core)
}

async fn http_request(socket_path: &std::path::Path, request: &str) -> (u16, String) {
    let mut stream = UnixStream::connect(socket_path).await.expect("connect");
    stream.write_all(request.as_bytes()).await.expect("write");

    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");

    let status_line = response.lines().next().unwrap_or_default();
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    (status, body)
}

#[tokio::test]
async fn status_and_refresh_endpoints_respond_over_unix_socket() {
    let store_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(store_dir.path().join("nexofs.sqlite3")).unwrap());
    let governor = Arc::new(ProviderApiGovernor::new());
    let (namespace_id, sync_core) = build_sync_core(store, governor.clone()).await;

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core);
    let event_bus = Arc::new(nexofs_sync_core::EventBus::new());
    let state = AppState::new(namespaces, Vec::new(), Vec::new(), governor, event_bus, 2 * 1024 * 1024 * 1024, std::env::temp_dir());

    let socket_dir = tempfile::tempdir().unwrap();
    let socket_path = socket_dir.path().join("control.sock");
    let serve_socket_path = socket_path.clone();
    tokio::spawn(async move {
        nexofs_local_api::serve(&serve_socket_path, state).await.unwrap();
    });

    // Aguarda o listener subir.
    for _ in 0..50 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let (status, body) = http_request(&socket_path, "GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status, 200);
    assert!(body.contains(&namespace_id.to_string()));

    let refresh_request = format!(
        "POST /v1/namespaces/{namespace_id}/refresh HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let (status, body) = http_request(&socket_path, &refresh_request).await;
    assert_eq!(status, 200, "corpo: {body}");
    assert!(body.contains("\"ok\""));

    let (status, _) = http_request(
        &socket_path,
        &format!("POST /v1/namespaces/{}/refresh HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", uuid::Uuid::new_v4()),
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn concurrent_refresh_clicks_on_same_namespace_are_deduplicated() {
    let store_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(store_dir.path().join("nexofs.sqlite3")).unwrap());
    let governor = Arc::new(ProviderApiGovernor::new());
    let (namespace_id, sync_core) = build_sync_core(store, governor).await;

    let call_count = Arc::new(AtomicUsize::new(0));
    let dedup: nexofs_api_governor::Deduplicator<NamespaceId, bool> = nexofs_api_governor::Deduplicator::new();

    let mut handles = Vec::new();
    for _ in 0..10 {
        let sync_core = sync_core.clone();
        let call_count = call_count.clone();
        let dedup = &dedup;
        handles.push(async move {
            dedup
                .run(namespace_id, move || {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    async move { sync_core.refresh_changes().await.is_ok() }
                })
                .await
        });
    }

    let results = futures_util::future::join_all(handles).await;
    assert!(results.iter().all(|r| *r));
    assert_eq!(call_count.load(Ordering::SeqCst), 1, "10 cliques concorrentes deveriam virar 1 execução real de refresh_changes");
}
