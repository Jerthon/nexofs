//! T3-06/§13 — dispatcher do journal contra um provedor real (aqui, o
//! `FakeProvider`, que implementa o mesmo contrato `CloudProvider` que o
//! adaptador OneDrive).

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::states::OperationType;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ItemKind, MoveItemRequest, ProviderAccountContext, UploadRequest};
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
async fn dispatch_uploads_a_new_top_level_file_and_marks_it_clean() {
    let (core, root, ..) = build_core().await;
    let item_id = core.create_local_item(root, "novo.txt", ItemKind::File).await.unwrap();
    let path = core.begin_write(item_id).await.unwrap();
    tokio::fs::write(&path, b"conteudo real").await.unwrap();
    core.update_local_size(item_id, 13).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();

    core.dispatch_pending_operations().await.unwrap();

    let item = core.get_item(item_id).await.unwrap().unwrap();
    assert!(item.remote_item_id.is_some(), "upload bem-sucedido deve gravar o remote_item_id retornado");
    assert_eq!(item.sync_state.as_deref(), Some("CLEAN"));

    let pending = core.pending_operations().await.unwrap();
    assert!(pending.iter().all(|op| op.item_id != Some(item_id)), "operação concluída não deve mais aparecer como pendente");
}

#[tokio::test]
async fn dispatch_creates_a_top_level_directory_remotely() {
    let (core, root, ..) = build_core().await;
    let item_id = core.create_local_item(root, "pasta", ItemKind::Directory).await.unwrap();

    core.dispatch_pending_operations().await.unwrap();

    let item = core.get_item(item_id).await.unwrap().unwrap();
    assert!(item.remote_item_id.is_some());
}

#[tokio::test]
async fn dispatch_uploads_a_file_created_inside_a_directory_created_in_a_previous_round() {
    let (core, root, ..) = build_core().await;
    let dir_id = core.create_local_item(root, "pasta", ItemKind::Directory).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let dir = core.get_item(dir_id).await.unwrap().unwrap();
    assert!(dir.remote_item_id.is_some(), "pré-condição: pasta já sincronizada antes do arquivo ser criado dentro dela");

    let file_id = core.create_local_item(dir_id, "dentro.txt", ItemKind::File).await.unwrap();
    let path = core.begin_write(file_id).await.unwrap();
    tokio::fs::write(&path, b"x").await.unwrap();
    core.stabilize_upload(file_id).await.unwrap();

    core.dispatch_pending_operations().await.unwrap();

    let file = core.get_item(file_id).await.unwrap().unwrap();
    assert!(file.remote_item_id.is_some(), "upload dentro de uma pasta já remota deve resolver o parent_remote_item_id e completar");
}

#[tokio::test]
async fn dispatch_defers_upload_when_parent_directory_is_not_yet_synced() {
    let (core, root, store, _provider) = build_core().await;
    // Cria a pasta e o arquivo dentro dela SEM despachar nada entre os dois
    // — a pasta ainda não tem `remote_item_id` quando o upload é tentado.
    let dir_id = core.create_local_item(root, "pasta", ItemKind::Directory).await.unwrap();
    let file_id = core.create_local_item(dir_id, "dentro.txt", ItemKind::File).await.unwrap();
    let path = core.begin_write(file_id).await.unwrap();
    tokio::fs::write(&path, b"x").await.unwrap();
    core.stabilize_upload(file_id).await.unwrap();

    // Upload (prioridade 20) é despachado antes de CreateDirectory
    // (prioridade 50) — a primeira rodada tenta o upload, não consegue
    // resolver o pai, adia; e cria a pasta. A segunda rodada finalmente
    // sobe o arquivo.
    core.dispatch_pending_operations().await.unwrap();
    let file_after_round_1 = core.get_item(file_id).await.unwrap().unwrap();
    assert!(file_after_round_1.remote_item_id.is_none(), "não deveria conseguir subir antes da pasta existir no remoto");
    let dir_after_round_1 = core.get_item(dir_id).await.unwrap().unwrap();
    assert!(dir_after_round_1.remote_item_id.is_some(), "criação da pasta não depende de nada, deveria ter completado na primeira rodada");

    // Em produção, o próximo tick da manutenção periódica só chega minutos
    // depois — no teste, adianta manualmente o relógio do backoff em vez de
    // dormir os 15s reais de `DEPENDENCY_RETRY_DELAY_SECS`.
    store
        .write(|tx| tx.execute("UPDATE operations SET next_attempt_at = 0 WHERE state = 'WAITING_RETRY'", []))
        .await
        .unwrap();

    core.dispatch_pending_operations().await.unwrap();
    let file_after_round_2 = core.get_item(file_id).await.unwrap().unwrap();
    assert!(file_after_round_2.remote_item_id.is_some(), "com a pasta já sincronizada, a segunda rodada deve completar o upload adiado");
}

#[tokio::test]
async fn dispatch_renames_a_synced_item_remotely() {
    let (core, root, .., provider) = build_core().await;
    let item_id = core.create_local_item(root, "antigo.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    core.rename_local_item(root, "antigo.txt", root, "novo.txt").await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    let item = core.get_item(item_id).await.unwrap().unwrap();
    assert_eq!(item.name, "novo.txt");
    let remote_item_id = item.remote_item_id.clone().unwrap();
    let remote = provider
        .get_item(nexofs_provider_api::GetItemRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            remote_item_id: nexofs_domain::RemoteItemId::from(remote_item_id),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(remote.name, "novo.txt", "o lado remoto deve refletir o novo nome");

    let pending = core.pending_operations().await.unwrap();
    assert!(pending.iter().all(|op| op.operation_type != OperationType::RenameItem || op.item_id != Some(item_id)));
}

#[tokio::test]
async fn dispatch_deletes_a_synced_item_remotely_and_removes_it_from_the_local_index() {
    let (core, root, _store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "apagar.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let remote_item_id = core.get_item(item_id).await.unwrap().unwrap().remote_item_id.unwrap();

    core.delete_local_item(root, "apagar.txt", ItemKind::File).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    assert!(core.get_item(item_id).await.unwrap().is_none(), "exclusão confirmada no remoto deve remover o item do índice local por completo");
    let remote = provider
        .get_item(nexofs_provider_api::GetItemRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            remote_item_id: nexofs_domain::RemoteItemId::from(remote_item_id),
        })
        .await
        .unwrap();
    assert!(remote.is_none(), "o item também deve estar excluído do lado remoto");
}

/// Bug real (T3-08): o dispatcher derivava `base_remote_version` relendo
/// `items.remote_version` no momento do despacho, em vez de usar o valor
/// congelado em `operations.base_remote_version` no momento do enqueue.
/// Entre o enqueue e o despacho, um `refresh_changes` incidental (outro
/// cliente mudando o mesmo arquivo) avançava `items.remote_version` — e
/// reler esse valor já atualizado fazia o `If-Match` "acertar" por engano,
/// mascarando silenciosamente o próprio conflito que o controle otimista de
/// versão existe para detectar.
#[tokio::test]
async fn dispatch_uses_the_base_version_frozen_at_enqueue_time_not_one_bumped_by_a_delta_in_between() {
    let (core, root, _store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "compartilhado.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let remote_item_id = core.get_item(item_id).await.unwrap().unwrap().remote_item_id.unwrap();

    // Nova escrita local — enfileira o upload com `base_remote_version`
    // congelado na versão atual (a única que existe até aqui).
    let path = core.begin_write(item_id).await.unwrap();
    tokio::fs::write(&path, b"minha mudanca local").await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();

    // Outro cliente muda o arquivo remotamente ANTES do despacho rodar, e um
    // refresh incidental traz essa mudança para `items.remote_version` —
    // mas não para `local_states.base_remote_version`, que continua
    // apontando para a versão que a edição local realmente conhecia.
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
    core.refresh_changes().await.unwrap();

    core.dispatch_pending_operations().await.unwrap();

    let item_after = core.get_item(item_id).await.unwrap().unwrap();
    assert_eq!(
        item_after.sync_state.as_deref(),
        Some("CONFLICT"),
        "reler items.remote_version no despacho mascararia este conflito — deve usar o valor congelado no enqueue"
    );
    let remote = provider
        .get_item(nexofs_provider_api::GetItemRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            remote_item_id: nexofs_domain::RemoteItemId::from(remote_item_id),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(remote.name, "compartilhado.txt");
    // O download comprova que a mudança do outro cliente não foi sobrescrita.
    let mut content = Vec::new();
    let mut handle = provider
        .open_download(nexofs_provider_api::DownloadRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            remote_item_id: nexofs_domain::RemoteItemId::from(remote.remote_item_id.0.clone()),
            range: None,
        })
        .await
        .unwrap();
    tokio::io::AsyncReadExt::read_to_end(&mut handle.reader, &mut content).await.unwrap();
    assert_eq!(content, b"mudanca de outro cliente", "upload não pode ter sobrescrito silenciosamente a mudança remota mais nova");
}

/// Mesma classe de bug do teste acima, mas para exclusão: `delete_local_item`
/// derivava `base_remote_version` de `items.remote_version` (que um
/// `refresh_changes` incidental pode ter avançado) em vez do valor congelado
/// em `local_states.base_remote_version` — mascarando um
/// `LocalDeletedRemoteModified` real como uma exclusão bem-sucedida.
#[tokio::test]
async fn deleting_after_an_incidental_refresh_still_detects_the_remote_change_as_a_conflict() {
    let (core, root, store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "apagar.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    // Outro cliente muda o conteúdo remotamente, e um refresh incidental
    // traz essa mudança para `items.remote_version` antes da exclusão local.
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
    core.refresh_changes().await.unwrap();

    core.delete_local_item(root, "apagar.txt", ItemKind::File).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    assert!(
        core.get_item(item_id).await.unwrap().is_some(),
        "exclusão baseada numa versão obsoleta não pode ter sido concluída — o item deve continuar no índice, em conflito"
    );
    let conflict_type: String = store
        .read(move |conn| {
            conn.query_row(
                "SELECT conflict_type FROM conflicts WHERE item_id = ?1 AND state = 'OPEN'",
                [item_id.to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(conflict_type, "LOCAL_DELETED_REMOTE_MODIFIED");
}

/// T3-08/SPEC §18.2 (segunda cláusula): uma exclusão remota que chega via
/// delta para um item ainda `Dirty` localmente não pode tombstoneá-lo — isso
/// faria a edição do usuário desaparecer silenciosamente da árvore antes de
/// qualquer upload ter a chance de detectar o conflito.
#[tokio::test]
async fn a_remote_delete_arriving_for_a_dirty_item_creates_a_conflict_instead_of_hiding_it() {
    let (core, root, store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "editando.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let remote_item_id = core.get_item(item_id).await.unwrap().unwrap().remote_item_id.unwrap();
    // Estabelece o cursor "a partir de agora" antes da edição local — sem
    // isso, o próximo `refresh_changes` seria o primeiro da vida do
    // namespace e pularia todo o histórico existente (`latest_only`,
    // FR-IDX-004), incluindo o tombstone que este teste precisa aplicar.
    core.refresh_changes().await.unwrap();

    // Edição local em andamento, ainda não estabilizada/despachada.
    let path = core.begin_write(item_id).await.unwrap();
    tokio::fs::write(&path, b"edicao local em andamento").await.unwrap();

    // Outro cliente apaga o arquivo remotamente; o refresh traz o tombstone.
    provider
        .delete_item(nexofs_provider_api::DeleteItemRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            remote_item_id: nexofs_domain::RemoteItemId::from(remote_item_id),
            base_remote_version: None,
        })
        .await
        .unwrap();
    core.refresh_changes().await.unwrap();

    let item_after = core.get_item(item_id).await.unwrap();
    assert!(item_after.is_some(), "o item não pode sumir da árvore enquanto a edição local não foi enviada");
    assert_eq!(item_after.unwrap().sync_state.as_deref(), Some("CONFLICT"));

    // O conteúdo dirty continua íntegro e legível.
    let hydrated_path = core.open_and_hydrate(item_id).await.unwrap();
    assert_eq!(tokio::fs::read(&hydrated_path).await.unwrap(), b"edicao local em andamento");

    let conflict_type: String = store
        .read(move |conn| {
            conn.query_row(
                "SELECT conflict_type FROM conflicts WHERE item_id = ?1 AND state = 'OPEN'",
                [item_id.to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(conflict_type, "REMOTE_DELETED_LOCAL_MODIFIED");
}

#[tokio::test]
async fn dispatch_blocks_on_conflict_instead_of_overwriting_a_newer_remote_version() {
    let (core, root, store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "compartilhado.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let item = core.get_item(item_id).await.unwrap().unwrap();
    let remote_item_id = item.remote_item_id.clone().unwrap();

    // Simula outro cliente alterando o mesmo arquivo remotamente entre o
    // upload original e a próxima escrita local.
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

    // Nova escrita local, baseada na versão antiga (`base_remote_version`
    // gravado na transição Clean→Dirty original, agora obsoleto).
    let path = core.begin_write(item_id).await.unwrap();
    tokio::fs::write(&path, b"minha mudanca local").await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    let operation_id: String = store
        .read(move |conn| {
            conn.query_row(
                "SELECT operation_id FROM operations WHERE item_id = ?1 AND operation_type = 'UPLOAD_FILE' ORDER BY rowid DESC LIMIT 1",
                [item_id.to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    let state: String = store
        .read(move |conn| conn.query_row("SELECT state FROM operations WHERE operation_id = ?1", [operation_id], |row| row.get(0)))
        .await
        .unwrap();
    assert_eq!(state, "BLOCKED_BY_CONFLICT", "conflito de versão não pode ser resolvido sozinho — precisa ficar bloqueado, nunca sobrescrever");

    // O conteúdo local não pode ter sido perdido — só passa a existir um
    // conflito estruturado (T3-08), nunca marcado como já sincronizado.
    let item_after = core.get_item(item_id).await.unwrap().unwrap();
    assert_eq!(item_after.sync_state.as_deref(), Some("CONFLICT"), "escrita local preservada — vira conflito estruturado, não o conteúdo");
    assert_eq!(tokio::fs::read(&path).await.unwrap(), b"minha mudanca local");

    let conflict_type: String = store
        .read(move |conn| {
            conn.query_row(
                "SELECT conflict_type FROM conflicts WHERE item_id = ?1 AND state = 'OPEN'",
                [item_id.to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(conflict_type, "CONTENT_CHANGED_BOTH_SIDES");
    let _ = remote_item_id;
}

/// T6-04/SPEC §26.3 ("rename concorrente") — mesma garantia do teste acima
/// (`dispatch_blocks_on_conflict_instead_of_overwriting_a_newer_remote_version`),
/// só que para `RenameItem` em vez de `UploadFile`: outro cliente move/
/// renomeia o mesmo arquivo remotamente entre o upload original e o
/// renomeio local — o dispatcher precisa detectar a versão obsoleta e
/// bloquear, nunca aplicar o renomeio local por cima silenciosamente.
#[tokio::test]
async fn dispatch_blocks_a_rename_when_the_remote_item_was_renamed_concurrently() {
    let (core, root, store, provider) = build_core().await;
    let item_id = core.create_local_item(root, "original.txt", ItemKind::File).await.unwrap();
    core.begin_write(item_id).await.unwrap();
    core.stabilize_upload(item_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    let item = core.get_item(item_id).await.unwrap().unwrap();
    let remote_item_id = item.remote_item_id.clone().unwrap();

    // Outro cliente renomeia o mesmo arquivo remotamente — avança a versão
    // remota para além da que o índice local ainda conhece.
    provider
        .move_item(MoveItemRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            remote_item_id: nexofs_domain::RemoteItemId::from(remote_item_id),
            new_parent_remote_item_id: None,
            new_name: Some("renomeado_por_outro_cliente.txt".to_string()),
            base_remote_version: None,
        })
        .await
        .unwrap();

    // Renomeio local, enfileirado com a `base_remote_version` antiga
    // (capturada no upload original, agora obsoleta).
    core.rename_local_item(root, "original.txt", root, "renomeado_localmente.txt").await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    let operation_id: String = store
        .read(move |conn| {
            conn.query_row(
                "SELECT operation_id FROM operations WHERE item_id = ?1 AND operation_type = 'RENAME_ITEM' ORDER BY rowid DESC LIMIT 1",
                [item_id.to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    let state: String =
        store.read(move |conn| conn.query_row("SELECT state FROM operations WHERE operation_id = ?1", [operation_id], |row| row.get(0))).await.unwrap();
    assert_eq!(state, "BLOCKED_BY_CONFLICT", "renomeio concorrente precisa ficar bloqueado, nunca aplicado silenciosamente por cima do nome do outro cliente");

    // O renomeio local pretendido não vaza para o índice como se já
    // tivesse sido aplicado — vira conflito estruturado (T3-08), como o
    // caso de conteúdo.
    let item_after = core.get_item(item_id).await.unwrap().unwrap();
    assert_eq!(item_after.sync_state.as_deref(), Some("CONFLICT"));
}
