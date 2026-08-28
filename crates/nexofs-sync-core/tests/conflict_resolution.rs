//! T4-12/T4-13 — resolução completa de conflitos (`KeepLocal`, `KeepRemote`,
//! `KeepBoth`, `DismissTemporarily`) para os três tipos detectados desde
//! T3-08.

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::states::ConflictResolution;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, RemoteItemId, SecretToken};
use nexofs_provider_api::{CloudProvider, DeleteItemRequest, DownloadRequest, GetItemRequest, ItemKind, MoveItemRequest, ProviderAccountContext, UploadRequest};
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

async fn build_core() -> (Arc<SyncCore>, nexofs_domain::ItemId, Arc<nexofs_metadata_store::MetadataStore>, Arc<FakeProvider>) {
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

/// Reproduz o mesmo cenário de conflito de `dispatch.rs`'s
/// `dispatch_blocks_on_conflict_instead_of_overwriting_a_newer_remote_version`,
/// mas agora resolvendo com cada uma das opções.
async fn setup_content_conflict(core: &Arc<SyncCore>, root: nexofs_domain::ItemId, provider: &Arc<FakeProvider>) -> nexofs_domain::ItemId {
    let item_id = core.create_local_item(root, "compartilhado.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    provider
        .upload(UploadRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            parent_remote_item_id: None,
            name: "compartilhado.txt".to_string(),
            size_bytes: 1,
            base_remote_version: None,
            content: Box::pin(b"mudanca de outro cliente".as_slice()),
            resumable_session_token: None,
        })
        .await
        .unwrap();

    let path = core.begin_write(item_id).await.unwrap();
    tokio::fs::write(&path, b"minha mudanca local").await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    assert_eq!(core.get_item(item_id).await.unwrap().unwrap().sync_state.as_deref(), Some("CONFLICT"), "pré-condição: conflito detectado");
    item_id
}

async fn open_conflict_id(store: &nexofs_metadata_store::MetadataStore, item_id: nexofs_domain::ItemId) -> nexofs_sync_core::ConflictId {
    let s: String = store
        .read(move |conn| conn.query_row("SELECT conflict_id FROM conflicts WHERE item_id = ?1 AND state = 'OPEN'", [item_id.to_string()], |row| row.get(0)))
        .await
        .unwrap();
    nexofs_sync_core::ConflictId(uuid::Uuid::parse_str(&s).unwrap())
}

async fn download_remote_content(provider: &Arc<FakeProvider>, remote_item_id: String) -> Vec<u8> {
    let mut handle = provider
        .open_download(DownloadRequest { account: account_ctx(), namespace_remote_id: "fake-namespace".to_string(), remote_item_id: RemoteItemId::from(remote_item_id), range: None })
        .await
        .unwrap();
    let mut buf = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut handle.reader, &mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn keep_local_overwrites_the_newer_remote_version_after_fetching_its_real_etag() {
    let (core, root, store, provider) = build_core().await;
    let item_id = setup_content_conflict(&core, root, &provider).await;
    let conflict_id = open_conflict_id(&store, item_id).await;

    core.resolve_conflict(conflict_id, ConflictResolution::KeepLocal).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    let item = core.get_item(item_id).await.unwrap().unwrap();
    assert_eq!(item.sync_state.as_deref(), Some("CLEAN"), "upload forçado deve ter completado");
    let remote_content = download_remote_content(&provider, item.remote_item_id.unwrap()).await;
    assert_eq!(remote_content, b"minha mudanca local", "conteúdo local deveria ter vencido de propósito");

    let conflicts_left: i64 = store.read(|conn| conn.query_row("SELECT COUNT(*) FROM conflicts WHERE state = 'OPEN'", [], |row| row.get(0))).await.unwrap();
    assert_eq!(conflicts_left, 0);
}

#[tokio::test]
async fn keep_remote_discards_the_local_edit_and_rehydrates_the_remote_content() {
    let (core, root, store, provider) = build_core().await;
    let item_id = setup_content_conflict(&core, root, &provider).await;
    let conflict_id = open_conflict_id(&store, item_id).await;

    core.resolve_conflict(conflict_id, ConflictResolution::KeepRemote).await.unwrap();

    let item = core.get_item(item_id).await.unwrap().unwrap();
    assert_eq!(item.sync_state.as_deref(), Some("CLEAN"));
    let hydrated_path = core.open_and_hydrate(item_id).await.unwrap();
    assert_eq!(tokio::fs::read(&hydrated_path).await.unwrap(), b"mudanca de outro cliente", "edição local deve ter sido descartada");
}

#[tokio::test]
async fn keep_both_uploads_the_local_edit_under_a_new_name_and_leaves_the_original_as_remote() {
    let (core, root, store, provider) = build_core().await;
    let item_id = setup_content_conflict(&core, root, &provider).await;
    let conflict_id = open_conflict_id(&store, item_id).await;

    core.resolve_conflict(conflict_id, ConflictResolution::KeepBoth).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    let original = core.get_item(item_id).await.unwrap().unwrap();
    assert_eq!(original.sync_state.as_deref(), Some("CLEAN"));
    let original_hydrated = core.open_and_hydrate(item_id).await.unwrap();
    assert_eq!(tokio::fs::read(&original_hydrated).await.unwrap(), b"mudanca de outro cliente");

    let siblings = core.list_children(root).await.unwrap();
    let copy = siblings.iter().find(|i| i.item_id != item_id).expect("deveria existir um segundo arquivo");
    assert!(copy.name.starts_with("compartilhado (conflito local "), "nome inesperado: {}", copy.name);
    assert!(copy.name.ends_with(".txt"));
    assert!(copy.remote_item_id.is_some(), "a cópia nova deve ter sido enviada normalmente");
    let copy_content = download_remote_content(&provider, copy.remote_item_id.clone().unwrap()).await;
    assert_eq!(copy_content, b"minha mudanca local");
}

#[tokio::test]
async fn dismiss_temporarily_leaves_the_conflict_open_and_the_item_protected() {
    let (core, root, store, provider) = build_core().await;
    let item_id = setup_content_conflict(&core, root, &provider).await;
    let conflict_id = open_conflict_id(&store, item_id).await;

    core.resolve_conflict(conflict_id, ConflictResolution::DismissTemporarily).await.unwrap();

    let (state, resolution): (String, Option<String>) = store
        .read(move |conn| conn.query_row("SELECT state, resolution FROM conflicts WHERE conflict_id = ?1", [conflict_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?))))
        .await
        .unwrap();
    assert_eq!(state, "OPEN", "conflito adiado continua aberto — T4-13");
    assert_eq!(resolution.as_deref(), Some("DISMISS_TEMPORARILY"));
    assert_eq!(core.get_item(item_id).await.unwrap().unwrap().sync_state.as_deref(), Some("CONFLICT"), "item continua protegido de eviction");

    // Resolvendo de verdade depois ainda funciona sobre o mesmo conflito.
    core.resolve_conflict(conflict_id, ConflictResolution::KeepRemote).await.unwrap();
    assert_eq!(core.get_item(item_id).await.unwrap().unwrap().sync_state.as_deref(), Some("CLEAN"));
}

#[tokio::test]
async fn local_deleted_remote_modified_keep_local_deletes_for_real_using_the_fresh_version() {
    let (core, root, store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "apagar.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let remote_item_id = core.get_item(item_id).await.unwrap().unwrap().remote_item_id.unwrap();

    provider
        .upload(UploadRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            parent_remote_item_id: None,
            name: "apagar.txt".to_string(),
            size_bytes: 1,
            base_remote_version: None,
            content: Box::pin(b"mudanca de outro cliente".as_slice()),
            resumable_session_token: None,
        })
        .await
        .unwrap();

    core.delete_local_item(root, "apagar.txt", ItemKind::File).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let conflict_id = open_conflict_id(&store, item_id).await;

    core.resolve_conflict(conflict_id, ConflictResolution::KeepLocal).await.unwrap();

    assert!(core.get_item(item_id).await.unwrap().is_none(), "item deve ter sumido do índice local após a exclusão de verdade");
    let remote = provider
        .get_item(GetItemRequest { account: account_ctx(), namespace_remote_id: "fake-namespace".to_string(), remote_item_id: RemoteItemId::from(remote_item_id) })
        .await
        .unwrap();
    assert!(remote.is_none(), "exclusão remota precisa ter sido executada de verdade, mesmo com a versão base obsoleta");
}

/// Bug real de produção: um `RENAME_ITEM`/`MOVE_ITEM` bloqueado por
/// `ContentChangedBothSides` (mesmo `ConflictType` de um upload — o Graph
/// não distingue eTag velho num PATCH de posição de eTag velho num upload
/// de conteúdo) resolvido com `KeepLocal` enfileirava um `UploadFile` sem
/// nenhum conteúdo dirty para enviar (renomear não modifica bytes) — a
/// operação ficava presa em `WAITING_RETRY` para sempre, pois o arquivo que
/// ela esperava nunca existiria. A resolução precisa reencaminhar o
/// rename/move em vez de um upload.
#[tokio::test]
async fn keep_local_on_a_blocked_rename_redispatches_the_rename_instead_of_an_upload() {
    let (core, root, store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "original.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let remote_item_id = core.get_item(item_id).await.unwrap().unwrap().remote_item_id.clone().unwrap();

    // Outro cliente renomeia o mesmo arquivo remotamente — avança a versão
    // remota para além da que o índice local ainda conhece.
    provider
        .move_item(MoveItemRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            remote_item_id: RemoteItemId::from(remote_item_id),
            new_parent_remote_item_id: None,
            new_name: Some("renomeado_por_outro_cliente.txt".to_string()),
            base_remote_version: None,
        })
        .await
        .unwrap();

    core.rename_local_item(root, "original.txt", root, "renomeado_localmente.txt").await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    let conflict_id = open_conflict_id(&store, item_id).await;
    core.resolve_conflict(conflict_id, ConflictResolution::KeepLocal).await.unwrap();

    // Já existe um UPLOAD_FILE `COMPLETED` da criação original do arquivo —
    // o que a resolução não pode ter feito é abrir um *novo*, pendente, que
    // ficaria preso esperando um arquivo dirty que nunca vai existir.
    let pending_upload_ops: i64 = store
        .read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM operations WHERE item_id = ?1 AND operation_type = 'UPLOAD_FILE' AND state <> 'COMPLETED'",
                [item_id.to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(pending_upload_ops, 0, "renomear não modifica bytes — não deveria nunca gerar um UploadFile pendente");

    core.dispatch_pending_operations().await.unwrap();

    let item = core.get_item(item_id).await.unwrap().unwrap();
    assert_eq!(item.sync_state.as_deref(), Some("CLEAN"), "o rename reencaminhado deveria ter completado");
    assert_eq!(item.name, "renomeado_localmente.txt", "nome local deveria ter vencido, como pedido");
}

/// Bug real de produção: uma exclusão local (`DELETE_ITEM`) que ficou
/// `BLOCKED_BY_CONFLICT` sem nenhuma linha correspondente em `conflicts`
/// (por exemplo, de antes do registro de conflitos existir para esse tipo
/// de operação) deixava o item escondido da listagem (`DELETED_LOCALLY`)
/// para sempre — a aba "Conflitos" nunca tinha o que mostrar, então o
/// usuário não tinha como decidir "apagar de verdade" ou "restaurar".
#[tokio::test]
async fn backfill_recovers_a_delete_blocked_without_a_conflict_row_and_makes_it_resolvable() {
    let (core, root, store, _provider) = build_core().await;
    let item_id = core.create_local_item(root, "escondido.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    // Simula a anomalia real: a operação já está bloqueada por conflito,
    // mas nenhum registro em `conflicts` foi criado para ela (exatamente o
    // estado encontrado em produção, de antes do registro existir).
    let item_id_s = item_id.to_string();
    store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO operations (operation_id, namespace_id, item_id, operation_type, state, priority, idempotency_key, attempt_count, payload_json, last_error_message, created_at, updated_at) \
                 VALUES (lower(hex(randomblob(16))), (SELECT namespace_id FROM items WHERE item_id = ?1), ?1, 'DELETE_ITEM', 'BLOCKED_BY_CONFLICT', 5, 'delete:teste:' || ?1, 0, '{}', 'eTag mismatch simulado', 0, 0)",
                rusqlite::params![item_id_s],
            )?;
            tx.execute("UPDATE local_states SET sync_state = 'DELETED_LOCALLY', updated_at = 0 WHERE item_id = ?1", rusqlite::params![item_id_s])
        })
        .await
        .unwrap();
    assert!(core.list_children(root).await.unwrap().is_empty(), "pré-condição: item escondido pela exclusão local não confirmada");

    let backfilled = core.backfill_missing_delete_conflicts().await.unwrap();
    assert_eq!(backfilled, 1);

    let conflicts = core.list_conflicts().await.unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].item_id, item_id);
    assert_eq!(conflicts[0].conflict_type, nexofs_domain::states::ConflictType::LocalDeletedRemoteModified);

    // Agora resolvível normalmente pela mesma UI/fluxo de qualquer conflito.
    core.resolve_conflict(conflicts[0].conflict_id, ConflictResolution::KeepRemote).await.unwrap();
    assert!(!core.list_children(root).await.unwrap().is_empty(), "KeepRemote deveria ter restaurado o item à listagem");

    // Rodar de novo não duplica nada — nada mais para reconciliar.
    assert_eq!(core.backfill_missing_delete_conflicts().await.unwrap(), 0);
}

#[tokio::test]
async fn remote_deleted_local_modified_keep_local_recreates_the_item_from_scratch() {
    let (core, root, store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "editando.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    core.refresh_changes().await.unwrap();
    let remote_item_id = core.get_item(item_id).await.unwrap().unwrap().remote_item_id.unwrap();

    let path = core.begin_write(item_id).await.unwrap();
    tokio::fs::write(&path, b"edicao local em andamento").await.unwrap();

    provider.delete_item(DeleteItemRequest { account: account_ctx(), namespace_remote_id: "fake-namespace".to_string(), remote_item_id: RemoteItemId::from(remote_item_id), base_remote_version: None }).await.unwrap();
    core.refresh_changes().await.unwrap();
    let conflict_id = open_conflict_id(&store, item_id).await;

    core.resolve_conflict(conflict_id, ConflictResolution::KeepLocal).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    let item = core.get_item(item_id).await.unwrap().unwrap();
    assert!(item.remote_item_id.is_some(), "deveria ter sido recriado remotamente do zero");
    let remote_content = download_remote_content(&provider, item.remote_item_id.unwrap()).await;
    assert_eq!(remote_content, b"edicao local em andamento");
}
