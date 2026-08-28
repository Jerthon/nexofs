//! Fase 3 — copy-on-write (T3-01), operações FUSE de escrita no nível do
//! `SyncCore` (T3-02) e journal de operações (T3-03).

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::states::{OperationState, OperationType};
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ItemKind, ProviderAccountContext, UploadRequest};
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{SyncCore, SyncCoreContext, SyncError};
use std::sync::Arc;

fn account_ctx() -> ProviderAccountContext {
    ProviderAccountContext {
        account_id: AccountId::new(),
        provider_account_id: "fake-account".to_string(),
        tenant_id: None,
        access_token: SecretToken::new("token"),
    }
}

/// Núcleo vazio, sem nenhum conteúdo pré-existente no provider — usado pelos
/// testes que só exercitam criação/escrita puramente local.
async fn build_core() -> (Arc<SyncCore>, nexofs_domain::ItemId) {
    let dir = tempfile::tempdir().unwrap();
    // Mantém `dir` vivo pelo tempo do teste vazando o TempDir — os testes
    // deste arquivo só precisam do caminho, nunca de limpeza automática.
    let dir = Box::leak(Box::new(dir));
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
    let core = Arc::new(SyncCore::new(store, provider, governor, cache, overlay, account_ctx(), ctx));
    let root = core.bootstrap_root().await.unwrap();
    (core, root)
}

/// Núcleo com um único arquivo já sincronizado (criado direto no
/// `FakeProvider` e indexado via `list_children`) — usado pelos testes que
/// exercitam exclusão/renomeio de um item com `remote_item_id` real, onde a
/// operação correspondente só é enfileirada se houver contrapartida remota.
async fn build_core_with_remote_file(file_name: &str) -> (Arc<SyncCore>, nexofs_domain::ItemId, nexofs_domain::ItemId) {
    let dir = tempfile::tempdir().unwrap();
    let dir = Box::leak(Box::new(dir));
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));
    let namespaces = provider.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();

    provider
        .upload(UploadRequest {
            account: account_ctx(),
            namespace_remote_id: namespace_remote_id.clone(),
            parent_remote_item_id: None,
            name: file_name.to_string(),
            size_bytes: 5,
            base_remote_version: None,
            content: Box::pin(b"hello".as_slice()),
            resumable_session_token: None,
        })
        .await
        .unwrap();

    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    {
        let namespace_remote_id_for_row = namespace_remote_id.clone();
        store
            .write(move |tx| {
                tx.execute("INSERT INTO providers (provider_id, display_name, capabilities_json, created_at, updated_at) VALUES ('fake', 'Fake', '{}', 0, 0)", [])?;
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

    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id };
    let core = Arc::new(SyncCore::new(store, provider, governor, cache, overlay, account_ctx(), ctx));
    let root = core.bootstrap_root().await.unwrap();
    let item = core.lookup_child(root, file_name).await.unwrap().unwrap();
    (core, root, item.item_id)
}

#[tokio::test]
async fn create_local_file_is_dirty_from_creation_even_without_a_write() {
    let (core, root) = build_core().await;

    let item_id = core.create_local_item(root, "novo.txt", ItemKind::File).await.unwrap();

    let item = core.get_item(item_id).await.unwrap().unwrap();
    assert_eq!(item.sync_state.as_deref(), Some("DIRTY"));
    assert_eq!(item.size_bytes, 0);

    let listed = core.list_children(root).await.unwrap();
    assert!(listed.iter().any(|i| i.item_id == item_id), "arquivo novo deve aparecer na listagem");
}

#[tokio::test]
async fn creating_over_an_existing_name_fails_with_already_exists() {
    let (core, root) = build_core().await;
    core.create_local_item(root, "dup.txt", ItemKind::File).await.unwrap();

    let err = core.create_local_item(root, "DUP.txt", ItemKind::File).await.unwrap_err();
    assert!(matches!(err, SyncError::AlreadyExists));
}

#[tokio::test]
async fn write_then_stabilize_enqueues_exactly_one_upload_operation() {
    let (core, root) = build_core().await;
    let item_id = core.create_local_item(root, "doc.txt", ItemKind::File).await.unwrap();

    // Simula múltiplas escritas (`write()` do FUSE chamaria begin_write a
    // cada chamada e escreveria no caminho retornado).
    for _ in 0..5 {
        let path = core.begin_write(item_id).await.unwrap();
        tokio::fs::write(&path, b"conteudo final").await.unwrap();
    }
    core.update_local_size(item_id, 14).await.unwrap();

    // Estabilização dispara em release/fsync — chamar várias vezes (ex.:
    // fsync seguido de release) não deve duplicar a operação enfileirada
    // (SPEC §13.4: "várias escritas → um upload final").
    core.stabilize_upload(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();

    let pending = core.pending_operations().await.unwrap();
    let uploads: Vec<_> = pending.iter().filter(|op| op.operation_type == OperationType::UploadFile && op.item_id == Some(item_id)).collect();
    assert_eq!(uploads.len(), 1, "múltiplas estabilizações da mesma geração dirty devem colapsar em uma única operação");
    assert_eq!(uploads[0].state, OperationState::Pending);
}

#[tokio::test]
async fn reading_back_a_dirty_never_synced_file_returns_local_content_instead_of_failing() {
    let (core, root) = build_core().await;
    let item_id = core.create_local_item(root, "novo.txt", ItemKind::File).await.unwrap();
    let dirty_path = core.begin_write(item_id).await.unwrap();
    tokio::fs::write(&dirty_path, b"conteudo local").await.unwrap();

    // Bug real encontrado validando a Fase 3 no mount de verdade: `open()`
    // read-only (ex.: `cat`) de um arquivo recém-criado caía na branch de
    // hidratação remota — que falha com `InvalidOperation` para um item sem
    // `remote_item_id` — e o FUSE traduzia isso (incorretamente) como
    // `EISDIR`. `open_and_hydrate` precisa reconhecer conteúdo dirty antes
    // de cogitar rede.
    let opened_path = core.open_and_hydrate(item_id).await.unwrap();
    assert_eq!(opened_path, dirty_path);
    assert_eq!(tokio::fs::read(&opened_path).await.unwrap(), b"conteudo local");
}

#[tokio::test]
async fn mkdir_enqueues_create_directory_operation_immediately() {
    let (core, root) = build_core().await;
    let item_id = core.create_local_item(root, "pasta_nova", ItemKind::Directory).await.unwrap();

    let pending = core.pending_operations().await.unwrap();
    assert!(pending.iter().any(|op| op.operation_type == OperationType::CreateDirectory && op.item_id == Some(item_id)));
}

#[tokio::test]
async fn deleting_a_never_synced_file_removes_it_and_cancels_its_pending_upload() {
    let (core, root) = build_core().await;
    core.create_local_item(root, "efemero.txt", ItemKind::File).await.unwrap();
    let item_id = core.lookup_child(root, "efemero.txt").await.unwrap().unwrap().item_id;
    core.stabilize_upload(item_id).await.unwrap();
    assert!(!core.pending_operations().await.unwrap().is_empty());

    core.delete_local_item(root, "efemero.txt", ItemKind::File).await.unwrap();

    assert!(core.lookup_child(root, "efemero.txt").await.unwrap().is_none());
    assert!(core.get_item(item_id).await.unwrap().is_none(), "item nunca sincronizado deve sumir por completo, não virar tombstone");
    assert!(
        core.pending_operations().await.unwrap().iter().all(|op| op.item_id != Some(item_id)),
        "upload obsoleto de um create+delete deve ser cancelado (SPEC §13.4)"
    );
}

#[tokio::test]
async fn deleting_a_remote_backed_item_hides_it_and_queues_delete_operation() {
    let (core, root, item_id) = build_core_with_remote_file("existente.txt").await;

    core.delete_local_item(root, "existente.txt", ItemKind::File).await.unwrap();

    assert!(core.lookup_child(root, "existente.txt").await.unwrap().is_none(), "item apagado localmente some da listagem");
    assert!(!core.list_children(root).await.unwrap().iter().any(|i| i.item_id == item_id));
    let still_indexed = core.get_item(item_id).await.unwrap();
    assert!(still_indexed.is_some(), "tombstone local não remove o item do índice, só o esconde");

    let pending = core.pending_operations().await.unwrap();
    assert!(pending.iter().any(|op| op.operation_type == OperationType::DeleteItem && op.item_id == Some(item_id)));
}

#[tokio::test]
async fn renaming_twice_before_dispatch_collapses_into_a_single_operation_with_the_final_name() {
    let (core, root, item_id) = build_core_with_remote_file("a.txt").await;

    core.rename_local_item(root, "a.txt", root, "b.txt").await.unwrap();
    core.rename_local_item(root, "b.txt", root, "c.txt").await.unwrap();

    assert!(core.lookup_child(root, "c.txt").await.unwrap().is_some());
    assert!(core.lookup_child(root, "a.txt").await.unwrap().is_none());
    assert!(core.lookup_child(root, "b.txt").await.unwrap().is_none());

    let pending = core.pending_operations().await.unwrap();
    let renames: Vec<_> = pending.iter().filter(|op| op.item_id == Some(item_id) && op.operation_type == OperationType::RenameItem).collect();
    assert_eq!(renames.len(), 1, "renomes sucessivos antes do dispatcher devem colapsar em uma única operação");
}

#[tokio::test]
async fn recover_running_operations_moves_them_back_to_pending() {
    let (core, root) = build_core().await;
    let item_id = core.create_local_item(root, "x.txt", ItemKind::File).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    let op = core.pending_operations().await.unwrap().into_iter().find(|op| op.item_id == Some(item_id)).unwrap();
    core.mark_operation_running(op.operation_id).await.unwrap();

    let recovered = core.recover_running_operations().await.unwrap();
    assert_eq!(recovered, 1);

    let after = core.pending_operations().await.unwrap();
    assert!(after.iter().any(|op| op.item_id == Some(item_id) && op.state == OperationState::Pending));
}

/// T3-04/SPEC §16.2, terceiro gatilho: "5 segundos sem nova escrita".
/// `start_paused` deixa o `tokio::time::sleep` interno do debounce resolver
/// via avanço de relógio virtual, sem gastar 5s reais de wall-clock.
#[tokio::test(start_paused = true)]
async fn a_write_stabilizes_automatically_after_five_seconds_with_no_further_writes() {
    let (core, root) = build_core().await;
    let item_id = core.create_local_item(root, "arquivo.txt", ItemKind::File).await.unwrap();
    let path = core.begin_write(item_id).await.unwrap();
    tokio::fs::write(&path, b"conteudo").await.unwrap();
    core.update_local_size(item_id, 8).await.unwrap();
    core.schedule_write_idle_stabilization(item_id);

    // Pouco antes da janela vencer, nada deve ter sido enfileirado ainda.
    tokio::time::sleep(std::time::Duration::from_millis(4900)).await;
    let pending_before = core.pending_operations().await.unwrap();
    assert!(
        pending_before.iter().all(|op| op.item_id != Some(item_id)),
        "não deveria estabilizar antes da janela de 5s vencer"
    );

    // Passa a marca de 5s; espera em pequenos incrementos (em vez de um
    // único sleep) para dar ao executor chances suficientes de completar a
    // tarefa de fundo do debounce (que ainda faz um round-trip real pela
    // thread de escrita do SQLite) depois do relógio virtual avançar.
    let mut waited = std::time::Duration::ZERO;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        waited += std::time::Duration::from_millis(50);
        let pending = core.pending_operations().await.unwrap();
        if pending.iter().any(|op| op.item_id == Some(item_id) && op.operation_type == OperationType::UploadFile) {
            break;
        }
        assert!(waited < std::time::Duration::from_secs(10), "deveria ter estabilizado automaticamente pouco depois dos 5s de inatividade");
    }
}

/// T3-04/SPEC §16.2, quarto e último gatilho: "comando manual".
#[tokio::test]
async fn stabilize_all_dirty_items_stabilizes_every_dirty_item_in_the_namespace_at_once() {
    let (core, root) = build_core().await;
    let mut item_ids = Vec::new();
    for i in 0..5 {
        let id = core.create_local_item(root, &format!("arquivo-{i}.txt"), ItemKind::File).await.unwrap();
        let path = core.begin_write(id).await.unwrap();
        tokio::fs::write(&path, format!("conteudo {i}")).await.unwrap();
        item_ids.push(id);
    }
    // Nenhum foi estabilizado ainda — nem `flush`/`release` nem o debounce
    // de 5s foram acionados.
    let pending_before = core.pending_operations().await.unwrap();
    assert!(pending_before.is_empty(), "pré-condição: nada deveria estar no journal antes do comando manual");

    let stabilized = core.stabilize_all_dirty_items().await.unwrap();
    assert_eq!(stabilized, 5);

    let pending_after = core.pending_operations().await.unwrap();
    for id in &item_ids {
        assert!(
            pending_after.iter().any(|op| op.item_id == Some(*id) && op.operation_type == OperationType::UploadFile),
            "cada item dirty deveria ter sido estabilizado pelo comando manual"
        );
    }
}
