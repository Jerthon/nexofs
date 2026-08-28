//! T5-01/SPEC §20.3 — catálogo de endpoints além de status/refresh/ignore
//! (já cobertos em `http_api.rs`): contas, namespaces, operações
//! (listar/retry/cancel) e cache (consultar/limpar).

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, ItemId, NamespaceId, ProviderId, SecretToken};
use nexofs_local_api::{AccountSummary, AppState, NamespaceSummary};
use nexofs_provider_api::{CloudProvider, ItemKind, ProviderAccountContext};
use nexofs_provider_fake::{FakeProvider, FaultInjectingProvider};
use nexofs_sync_core::{EventBus, SyncCore, SyncCoreContext};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::net::UnixStream;

fn account_ctx() -> ProviderAccountContext {
    ProviderAccountContext {
        account_id: AccountId::new(),
        provider_account_id: "fake-account".to_string(),
        tenant_id: None,
        access_token: SecretToken::new("token"),
    }
}

async fn build_sync_core() -> (NamespaceId, Arc<SyncCore>, Arc<FaultInjectingProvider>, ItemId) {
    let store_dir = tempfile::tempdir().unwrap();
    let store_dir = Box::leak(Box::new(store_dir));
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(store_dir.path().join("nexofs.sqlite3")).unwrap());
    let governor = Arc::new(ProviderApiGovernor::new());
    let fake = Arc::new(FakeProvider::new());
    let provider = Arc::new(FaultInjectingProvider::new(fake as Arc<dyn CloudProvider>));
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
    let core = Arc::new(SyncCore::new(store, provider.clone() as Arc<dyn CloudProvider>, governor, cache, overlay, account_ctx(), ctx));
    let root = core.bootstrap_root().await.unwrap();
    (namespace_id, core, provider, root)
}

async fn serve_and_get_socket(state: AppState) -> std::path::PathBuf {
    let socket_dir = tempfile::tempdir().unwrap();
    let socket_dir = Box::leak(Box::new(socket_dir));
    let socket_path = socket_dir.path().join("control.sock");
    // Descarta o listener de verdade — só precisamos garantir que o
    // caminho não existe antes de `nexofs_local_api::serve` bindar nele;
    // ele mesmo remove/recria o arquivo.
    drop(UnixListener::bind(socket_dir.path().join("_probe.sock")));

    let path_for_task = socket_path.clone();
    tokio::spawn(async move {
        let _ = nexofs_local_api::serve(&path_for_task, state).await;
    });
    for _ in 0..50 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    socket_path
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

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
}

fn post(path: &str) -> String {
    format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

fn post_json(path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn delete(path: &str) -> String {
    format!("DELETE {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

#[tokio::test]
async fn accounts_and_namespaces_reflect_the_summaries_given_at_startup() {
    let (namespace_id, sync_core, _provider, _root) = build_sync_core().await;
    let account_id = AccountId::new();

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core);
    let accounts = vec![AccountSummary { account_id, provider_id: "onedrive".to_string(), display_name: "Conta Fake".to_string() }];
    let namespace_summaries = vec![NamespaceSummary {
        namespace_id,
        account_id,
        display_name: "Conta Fake".to_string(),
        mount_path: "/home/user/NexoFS/ContaFake".to_string(),
        mount_state: "MOUNTED".to_string(),
    }];
    let state = AppState::new(namespaces, accounts, namespace_summaries, Arc::new(ProviderApiGovernor::new()), Arc::new(EventBus::new()), 1024, std::env::temp_dir());
    let socket_path = serve_and_get_socket(state).await;

    let (status, body) = http_request(&socket_path, &get("/v1/accounts")).await;
    assert_eq!(status, 200);
    assert!(body.contains("Conta Fake") && body.contains("onedrive"));

    let (status, body) = http_request(&socket_path, &get("/v1/namespaces")).await;
    assert_eq!(status, 200);
    assert!(body.contains(&namespace_id.to_string()) && body.contains("MOUNTED"));
}

#[tokio::test]
async fn add_account_without_a_channel_configured_reports_not_supported() {
    let state = AppState::new(HashMap::new(), Vec::new(), Vec::new(), Arc::new(ProviderApiGovernor::new()), Arc::new(EventBus::new()), 1024, std::env::temp_dir());
    let socket_path = serve_and_get_socket(state).await;

    let (status, _) = http_request(&socket_path, &post("/v1/accounts/auth/start")).await;
    assert_eq!(status, 501, "sem `nexofsd` de verdade por trás, o endpoint deve dizer isso, não travar nem 404");
}

/// A API local não sabe autenticar/montar FUSE (isso é do `nexofsd::main`,
/// ver `handle_add_account_requests`) — este teste faz o papel dele: recebe
/// o pedido pelo canal e responde como se tivesse montado com sucesso, só
/// para provar que o round-trip HTTP -> canal -> `oneshot` -> HTTP funciona
/// e que `insert_mounted` deixa a conta nova visível em `GET /v1/namespaces`.
#[tokio::test]
async fn add_account_round_trips_through_the_channel_and_becomes_visible() {
    let namespace_id = NamespaceId::new();
    let account_id = AccountId::new();
    let new_namespace = NamespaceSummary {
        namespace_id,
        account_id,
        display_name: "Conta Nova".to_string(),
        mount_path: "/home/user/NexoFS/ContaNova".to_string(),
        mount_state: "MOUNTED".to_string(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let state = AppState::new(HashMap::new(), Vec::new(), Vec::new(), Arc::new(ProviderApiGovernor::new()), Arc::new(EventBus::new()), 1024, std::env::temp_dir())
        .with_add_account_channel(tx);

    let responder_state = state.clone();
    let responder_namespace = new_namespace.clone();
    tokio::spawn(async move {
        let request = rx.recv().await.unwrap();
        let (_namespace_id, sync_core, _provider, _root) = build_sync_core().await;
        let account = AccountSummary { account_id, provider_id: "onedrive".to_string(), display_name: "Conta Nova".to_string() };
        responder_state.insert_mounted(namespace_id, sync_core, account, responder_namespace.clone()).await;
        let _ = request.respond_to.send(Ok(responder_namespace));
    });

    let socket_path = serve_and_get_socket(state).await;
    let (status, body) = http_request(&socket_path, &post("/v1/accounts/auth/start")).await;
    assert_eq!(status, 200, "corpo: {body}");
    assert!(body.contains("Conta Nova"), "corpo: {body}");

    let (status, body) = http_request(&socket_path, &get("/v1/namespaces")).await;
    assert_eq!(status, 200);
    assert!(body.contains(&namespace_id.to_string()), "a conta adicionada em runtime deve aparecer em GET /v1/namespaces sem reiniciar: {body}");
}

#[tokio::test]
async fn a_waiting_network_operation_can_be_listed_and_retried_over_the_api() {
    let (namespace_id, sync_core, provider, root) = build_sync_core().await;
    let item_id = sync_core.create_local_item(root, "arquivo.txt", ItemKind::File).await.unwrap();
    sync_core.begin_write(item_id).await.unwrap();
    sync_core.stabilize_upload(item_id).await.unwrap();

    provider.queue_network_failures(1);
    sync_core.dispatch_pending_operations().await.unwrap();

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core.clone());
    let state = AppState::new(namespaces, Vec::new(), Vec::new(), Arc::new(ProviderApiGovernor::new()), Arc::new(EventBus::new()), 1024, std::env::temp_dir());
    let socket_path = serve_and_get_socket(state).await;

    let (status, body) = http_request(&socket_path, &get("/v1/operations")).await;
    assert_eq!(status, 200, "corpo: {body}");
    assert!(body.contains("WAITING_NETWORK") || body.contains("WaitingNetwork"), "corpo: {body}");

    let operation_id = sync_core.pending_operations().await.unwrap()[0].operation_id;
    let (status, body) = http_request(&socket_path, &post(&format!("/v1/operations/{operation_id}/retry"))).await;
    assert_eq!(status, 200, "corpo: {body}");

    let pending = sync_core.pending_operations().await.unwrap();
    assert_eq!(pending[0].state, nexofs_domain::states::OperationState::Pending, "retry deve destravar a operação para o próximo despacho imediatamente");
}

#[tokio::test]
async fn a_pending_operation_can_be_cancelled_over_the_api() {
    let (namespace_id, sync_core, _provider, root) = build_sync_core().await;
    sync_core.create_local_item(root, "pasta", ItemKind::Directory).await.unwrap();
    let operation_id = sync_core.pending_operations().await.unwrap()[0].operation_id;

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core.clone());
    let state = AppState::new(namespaces, Vec::new(), Vec::new(), Arc::new(ProviderApiGovernor::new()), Arc::new(EventBus::new()), 1024, std::env::temp_dir());
    let socket_path = serve_and_get_socket(state).await;

    let (status, _) = http_request(&socket_path, &post(&format!("/v1/operations/{operation_id}/cancel"))).await;
    assert_eq!(status, 200);

    let pending = sync_core.pending_operations().await.unwrap();
    assert!(pending.is_empty(), "uma operação cancelada não deve mais aparecer como pendente");

    let (status, _) = http_request(&socket_path, &post(&format!("/v1/operations/{operation_id}/cancel"))).await;
    assert_eq!(status, 404, "cancelar de novo uma operação já terminal deve reportar não encontrada");
}

#[tokio::test]
async fn cache_endpoint_reports_usage_and_cleanup_never_errors_with_nothing_to_evict() {
    let (namespace_id, sync_core, ..) = build_sync_core().await;

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core);
    let state = AppState::new(namespaces, Vec::new(), Vec::new(), Arc::new(ProviderApiGovernor::new()), Arc::new(EventBus::new()), 1024, std::env::temp_dir());
    let socket_path = serve_and_get_socket(state).await;

    let (status, body) = http_request(&socket_path, &get("/v1/cache")).await;
    assert_eq!(status, 200);
    assert!(body.contains(&namespace_id.to_string()));
    // T5-07: detalhamento por camada precisa estar na resposta, não só o total.
    assert!(body.contains("clean_bytes"));
    assert!(body.contains("dirty_bytes"));
    assert!(body.contains("overlay_bytes"));
    assert!(body.contains("partial_bytes"));

    let (status, _) = http_request(&socket_path, &post("/v1/cache/cleanup")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn diagnostics_package_includes_schema_and_integrity_and_is_saved_to_disk() {
    let (namespace_id, sync_core, ..) = build_sync_core().await;

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core);
    let namespace_summaries = vec![NamespaceSummary {
        namespace_id,
        account_id: AccountId::new(),
        display_name: "Conta Fake".to_string(),
        mount_path: "/home/user/NexoFS/ContaFake".to_string(),
        mount_state: "MOUNTED".to_string(),
    }];
    let diagnostics_dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        namespaces,
        Vec::new(),
        namespace_summaries,
        Arc::new(ProviderApiGovernor::new()),
        Arc::new(EventBus::new()),
        1024,
        diagnostics_dir.path().to_path_buf(),
    );
    let socket_path = serve_and_get_socket(state).await;

    let (status, body) = http_request(&socket_path, &post("/v1/diagnostics/package")).await;
    assert_eq!(status, 200, "corpo: {body}");
    assert!(body.contains("sqlite_integrity_ok") && body.contains("schema_version"), "corpo: {body}");
    // Não há aqui uma asserção de "nunca contém segredo": `diagnostics_snapshot`
    // nunca toca `ProviderAccountContext`/`SecretToken` (só schema/journal/
    // conflitos/cache), e `recent_daemon_logs` reflete o journald real da
    // máquina que roda o teste — testar contra o texto dele tornaria este
    // teste dependente do histórico do host, não do código.

    let saved: Vec<_> = std::fs::read_dir(diagnostics_dir.path()).unwrap().collect();
    assert_eq!(saved.len(), 1, "uma cópia do pacote deve ser salva em disco a cada chamada");
}

/// Regressão: `GET /v1/conflicts` (sem namespace na URL) precisa existir —
/// `nexofs-cli`/o backend do Tauri chamam este, não o namespace-scoped
/// `/v1/namespaces/{id}/conflicts`; a ausência dele virava um 404 visível
/// direto na tela de conflitos da UI.
#[tokio::test]
async fn global_conflicts_endpoint_exists_and_aggregates_across_namespaces() {
    let (namespace_id, sync_core, ..) = build_sync_core().await;

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core);
    let state = AppState::new(namespaces, Vec::new(), Vec::new(), Arc::new(ProviderApiGovernor::new()), Arc::new(EventBus::new()), 1024, std::env::temp_dir());
    let socket_path = serve_and_get_socket(state).await;

    let (status, body) = http_request(&socket_path, &get("/v1/conflicts")).await;
    assert_eq!(status, 200, "corpo: {body}");
    assert!(body.contains("\"conflicts\""), "corpo: {body}");
}

#[tokio::test]
async fn namespace_items_lists_the_root_by_default_and_reports_pin_state() {
    let (namespace_id, sync_core, _provider, root) = build_sync_core().await;
    let item_id = sync_core.create_local_item(root, "documento.txt", ItemKind::File).await.unwrap();
    sync_core.set_pin_state(item_id, nexofs_sync_core::PinState::Pinned).await.unwrap();

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core);
    let state = AppState::new(namespaces, Vec::new(), Vec::new(), Arc::new(ProviderApiGovernor::new()), Arc::new(EventBus::new()), 1024, std::env::temp_dir());
    let socket_path = serve_and_get_socket(state).await;

    let (status, body) = http_request(&socket_path, &get(&format!("/v1/namespaces/{namespace_id}/items"))).await;
    assert_eq!(status, 200, "corpo: {body}");
    assert!(body.contains("documento.txt") && body.contains("\"PINNED\""), "corpo: {body}");
}

/// T5-06: a tela de exclusões precisa listar, criar e remover — este teste
/// cobre o ciclo completo pela API HTTP (não só o núcleo, já coberto pelos
/// testes de `nexofs-sync-core`).
#[tokio::test]
async fn ignore_rules_can_be_created_listed_and_removed_over_the_api() {
    let (namespace_id, sync_core, _provider, _root) = build_sync_core().await;

    let mut namespaces = HashMap::new();
    namespaces.insert(namespace_id, sync_core);
    let state = AppState::new(namespaces, Vec::new(), Vec::new(), Arc::new(ProviderApiGovernor::new()), Arc::new(EventBus::new()), 1024, std::env::temp_dir());
    let socket_path = serve_and_get_socket(state).await;

    let (status, body) = http_request(&socket_path, &get(&format!("/v1/namespaces/{namespace_id}/ignore-rules"))).await;
    assert_eq!(status, 200, "corpo: {body}");
    assert!(body.contains("\"rules\":[]"), "esperava lista vazia, corpo: {body}");

    let (status, body) =
        http_request(&socket_path, &post_json(&format!("/v1/namespaces/{namespace_id}/ignore-rules"), r#"{"pattern":"node_modules/"}"#)).await;
    assert_eq!(status, 200, "corpo: {body}");

    let (status, body) = http_request(&socket_path, &get(&format!("/v1/namespaces/{namespace_id}/ignore-rules"))).await;
    assert_eq!(status, 200);
    assert!(body.contains("node_modules/") && body.contains("\"tier\":\"Account\""), "corpo: {body}");
    let rule_id = body
        .split("\"rule_id\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("resposta deveria trazer rule_id");

    let (status, _) = http_request(&socket_path, &delete(&format!("/v1/namespaces/{namespace_id}/ignore-rules/{rule_id}"))).await;
    assert_eq!(status, 200);

    let (status, body) = http_request(&socket_path, &get(&format!("/v1/namespaces/{namespace_id}/ignore-rules"))).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"rules\":[]"), "regra deveria ter sido removida, corpo: {body}");

    // Remover de novo (id já não existe) responde 404, não um falso sucesso.
    let (status, _) = http_request(&socket_path, &delete(&format!("/v1/namespaces/{namespace_id}/ignore-rules/{rule_id}"))).await;
    assert_eq!(status, 404);
}
