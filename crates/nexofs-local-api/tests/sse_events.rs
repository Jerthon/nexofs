//! T5-02/SPEC §20.4 — `GET /v1/events` precisa entregar eventos de verdade
//! pelo socket, não só existir: conecta, mantém a conexão aberta (SSE não
//! fecha como as demais respostas) e confirma que uma mudança real no
//! núcleo aparece do outro lado sem polling algum.

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_local_api::AppState;
use nexofs_provider_api::{CloudProvider, ProviderAccountContext};
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{EventBus, SyncCore, SyncCoreContext};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
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

async fn build_sync_core(event_bus: Arc<EventBus>) -> (NamespaceId, Arc<SyncCore>) {
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

    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id };
    let core = Arc::new(SyncCore::new(store, provider.clone() as Arc<dyn CloudProvider>, governor, cache, overlay, account_ctx(), ctx).with_event_bus(event_bus));
    core.bootstrap_root().await.unwrap();
    (namespace_id, core)
}

#[tokio::test]
async fn a_refresh_arrives_on_the_sse_stream_without_any_polling() {
    let event_bus = Arc::new(EventBus::new());
    let (namespace_id, sync_core) = build_sync_core(event_bus.clone()).await;

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core.clone());
    let state = AppState::new(namespaces, Vec::new(), Vec::new(), Arc::new(ProviderApiGovernor::new()), event_bus, 1024, std::env::temp_dir());

    let socket_dir = tempfile::tempdir().unwrap();
    let socket_path = socket_dir.path().join("control.sock");
    let path_for_task = socket_path.clone();
    tokio::spawn(async move {
        let _ = nexofs_local_api::serve(&path_for_task, state).await;
    });
    for _ in 0..50 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut stream = UnixStream::connect(&socket_path).await.unwrap();
    stream
        .write_all(b"GET /v1/events HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
        .await
        .unwrap();

    // Deixa o cabeçalho da resposta chegar antes de disparar o evento, só
    // para não depender de sorte na ordem de chegada dos bytes.
    tokio::time::sleep(Duration::from_millis(50)).await;

    sync_core.refresh_changes().await.unwrap();

    let mut accumulated = String::new();
    let mut buf = vec![0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !accumulated.contains("REFRESH_COMPLETED") {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(remaining > Duration::ZERO, "evento REFRESH_COMPLETED não chegou a tempo pelo stream SSE, recebido até agora: {accumulated}");
        let received = tokio::time::timeout(remaining, stream.read(&mut buf))
            .await
            .expect("leitura do stream SSE expirou")
            .unwrap();
        assert!(received > 0, "conexão SSE fechou antes do evento chegar, recebido até agora: {accumulated}");
        accumulated.push_str(&String::from_utf8_lossy(&buf[..received]));
    }
}
