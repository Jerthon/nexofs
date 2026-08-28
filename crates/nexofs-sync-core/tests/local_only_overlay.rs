//! T4-01/T4-05/T4-06/T4-07 — regras de exclusão gerando conteúdo `LocalOnly`
//! persistido no overlay, nunca gerando operação remota, visível na mesma
//! árvore (visão mesclada), e colisão com item remoto de mesmo nome.

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ItemKind, ProviderAccountContext};
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{RuleTier, SyncCore, SyncCoreContext};
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
async fn a_file_matching_an_ignore_rule_never_enqueues_any_remote_operation() {
    let (core, root, ..) = build_core().await;
    core.add_ignore_rule(RuleTier::TechProfile, "node_modules/").await.unwrap();

    let dir_id = core.create_local_item(root, "node_modules", ItemKind::Directory).await.unwrap();
    let file_id = core.create_local_item(dir_id, "pacote.js", ItemKind::File).await.unwrap();
    let path = core.begin_write(file_id).await.unwrap();
    tokio::fs::write(&path, b"conteudo do pacote").await.unwrap();
    core.update_local_size(file_id, 18).await.unwrap();
    core.stabilize_upload(file_id).await.unwrap();

    // Zero operações: nem CreateDirectory para a pasta, nem UploadFile para
    // o arquivo dentro dela (T4-01).
    let pending = core.pending_operations().await.unwrap();
    assert!(pending.is_empty(), "item LocalOnly não pode gerar nenhuma operação de journal: {pending:?}");

    core.dispatch_pending_operations().await.unwrap();
    let dir = core.get_item(dir_id).await.unwrap().unwrap();
    let file = core.get_item(file_id).await.unwrap().unwrap();
    assert!(dir.remote_item_id.is_none());
    assert!(file.remote_item_id.is_none());
    assert_eq!(dir.source_layer, "LOCAL_ONLY");
    assert_eq!(file.source_layer, "LOCAL_ONLY");

    // T4-06: visão mesclada — o item continua aparecendo normalmente na
    // listagem e é legível como qualquer outro arquivo.
    let listing = core.list_children(dir_id).await.unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "pacote.js");
    let hydrated_path = core.open_and_hydrate(file_id).await.unwrap();
    assert_eq!(tokio::fs::read(&hydrated_path).await.unwrap(), b"conteudo do pacote");
}

#[tokio::test]
async fn a_higher_precedence_exception_can_still_sync_a_specific_file_inside_an_excluded_tree() {
    let (core, root, ..) = build_core().await;
    core.add_ignore_rule(RuleTier::TechProfile, "vendor/").await.unwrap();
    core.add_ignore_rule(RuleTier::UserException, "!vendor/meu-fork/**").await.unwrap();

    let vendor_id = core.create_local_item(root, "vendor", ItemKind::Directory).await.unwrap();
    let normal_pkg = core.create_local_item(vendor_id, "outro-pacote.php", ItemKind::File).await.unwrap();
    let fork_dir = core.create_local_item(vendor_id, "meu-fork", ItemKind::Directory).await.unwrap();
    let fork_file = core.create_local_item(fork_dir, "index.php", ItemKind::File).await.unwrap();

    assert_eq!(core.get_item(normal_pkg).await.unwrap().unwrap().source_layer, "LOCAL_ONLY");
    assert_eq!(
        core.get_item(fork_file).await.unwrap().unwrap().source_layer,
        "LOCAL",
        "exceção explícita deve trazer o arquivo de volta para o fluxo normal de sincronização"
    );
}

#[tokio::test]
async fn deleting_a_local_only_item_removes_it_from_the_overlay_and_the_index() {
    let (core, root, ..) = build_core().await;
    core.add_ignore_rule(RuleTier::UserGlobal, "*.tmp").await.unwrap();

    let file_id = core.create_local_item(root, "rascunho.tmp", ItemKind::File).await.unwrap();
    let path = core.begin_write(file_id).await.unwrap();
    tokio::fs::write(&path, b"rascunho").await.unwrap();
    assert!(path.is_file());

    core.delete_local_item(root, "rascunho.tmp", ItemKind::File).await.unwrap();

    assert!(core.get_item(file_id).await.unwrap().is_none());
    assert!(!path.is_file(), "o arquivo no overlay deve ter sido removido junto com o item");
}

#[tokio::test]
async fn a_remote_item_with_the_same_name_as_a_local_only_item_becomes_a_collision_not_a_silent_duplicate() {
    let (core, root, store, provider) = build_core().await;
    core.add_ignore_rule(RuleTier::UserGlobal, "conflitante.txt").await.unwrap();
    let local_only_id = core.create_local_item(root, "conflitante.txt", ItemKind::File).await.unwrap();
    assert_eq!(core.get_item(local_only_id).await.unwrap().unwrap().source_layer, "LOCAL_ONLY");
    // Estabelece o cursor "a partir de agora" antes do upload do outro
    // cliente — do contrário o próximo `refresh_changes` seria o primeiro
    // da vida do namespace e pularia esse evento (`latest_only`).
    core.refresh_changes().await.unwrap();

    // Outro cliente cria um item remoto de mesmo nome, na raiz.
    provider
        .upload(nexofs_provider_api::UploadRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            parent_remote_item_id: None,
            name: "conflitante.txt".to_string(),
            size_bytes: 1,
            base_remote_version: None,
            content: Box::pin(b"outro conteudo, de outro cliente".as_slice()),
            resumable_session_token: None,
        })
        .await
        .unwrap();
    core.refresh_changes().await.unwrap();

    // Não pode ter criado uma segunda linha para o mesmo nome/pai.
    let count: i64 = store
        .read(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM items WHERE normalized_name = 'conflitante.txt'",
                [],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(count, 1, "não pode existir duplicata — a colisão deve virar conflito, não uma segunda linha");

    let conflict_type: String = store
        .read(move |conn| {
            conn.query_row(
                "SELECT conflict_type FROM conflicts WHERE item_id = ?1 AND state = 'OPEN'",
                [local_only_id.to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(conflict_type, "LOCAL_ONLY_REMOTE_COLLISION");
}

/// T4-04: um manifesto conhecido na raiz sugere o perfil correspondente,
/// mas nada é aplicado sozinho — só depois de `apply_ignore_profile`
/// (a "confirmação explícita" do SPEC §17.4) é que os padrões viram regras
/// de verdade e passam a valer para itens criados depois.
#[tokio::test]
async fn a_known_manifest_at_the_root_suggests_its_profile_but_never_applies_it_automatically() {
    let (core, root, ..) = build_core().await;

    let manifest_id = core.create_local_item(root, "package.json", ItemKind::File).await.unwrap();
    core.begin_write(manifest_id).await.unwrap();

    let suggestions = core.suggest_ignore_profiles().await.unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].name, "nodejs");

    // Antes da confirmação, node_modules ainda sincronizaria normalmente.
    let before_id = core.create_local_item(root, "node_modules", ItemKind::Directory).await.unwrap();
    assert_eq!(core.get_item(before_id).await.unwrap().unwrap().source_layer, "LOCAL");
    core.delete_local_item(root, "node_modules", ItemKind::Directory).await.unwrap();

    core.apply_ignore_profile(&suggestions[0]).await.unwrap();

    let after_id = core.create_local_item(root, "node_modules", ItemKind::Directory).await.unwrap();
    assert_eq!(core.get_item(after_id).await.unwrap().unwrap().source_layer, "LOCAL_ONLY", "depois de confirmado, o perfil precisa valer");

    // Perfil já aplicado não é sugerido de novo.
    assert!(core.suggest_ignore_profiles().await.unwrap().is_empty());
}

/// T4-08/FR-LOC-006: trazer uma subárvore `LocalOnly` de volta ao fluxo
/// normal — estimativa correta antes, upload de verdade depois.
#[tokio::test]
async fn migrating_a_local_only_subtree_back_to_normal_sync_uploads_everything_inside() {
    let (core, root, store, ..) = build_core().await;
    core.add_ignore_rule(RuleTier::TechProfile, "vendor/").await.unwrap();

    let vendor_id = core.create_local_item(root, "vendor", ItemKind::Directory).await.unwrap();
    let file_a = core.create_local_item(vendor_id, "a.php", ItemKind::File).await.unwrap();
    let path_a = core.begin_write(file_a).await.unwrap();
    tokio::fs::write(&path_a, b"1234567890").await.unwrap();
    core.update_local_size(file_a, 10).await.unwrap();
    let file_b = core.create_local_item(vendor_id, "b.php", ItemKind::File).await.unwrap();
    let path_b = core.begin_write(file_b).await.unwrap();
    tokio::fs::write(&path_b, b"12345").await.unwrap();
    core.update_local_size(file_b, 5).await.unwrap();

    let estimate = core.estimate_local_only_subtree(vendor_id).await.unwrap();
    assert_eq!(estimate.item_count, 3, "a própria pasta + os 2 arquivos");
    assert_eq!(estimate.total_bytes, 15);

    core.migrate_local_only_to_normal_sync(vendor_id).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    // Uploads despacham antes de CreateDirectory (prioridade 20 < 50),
    // adiam por não acharem a pasta ainda, e ganham um backoff real de 15s
    // (`DEPENDENCY_RETRY_DELAY_SECS`) — adianta manualmente em vez de
    // dormir de verdade, mesmo padrão de `dispatch_defers_upload_when_parent_directory_is_not_yet_synced`.
    store.write(|tx| tx.execute("UPDATE operations SET next_attempt_at = 0 WHERE state = 'WAITING_RETRY'", [])).await.unwrap();
    core.dispatch_pending_operations().await.unwrap(); // 2ª rodada: uploads dependiam da pasta

    let vendor = core.get_item(vendor_id).await.unwrap().unwrap();
    let a = core.get_item(file_a).await.unwrap().unwrap();
    let b = core.get_item(file_b).await.unwrap().unwrap();
    assert_eq!(vendor.source_layer, "LOCAL");
    assert!(vendor.remote_item_id.is_some());
    assert_eq!(a.source_layer, "LOCAL");
    assert!(a.remote_item_id.is_some(), "upload da migração precisa ter completado de verdade");
    assert_eq!(b.source_layer, "LOCAL");
    assert!(b.remote_item_id.is_some());
}

/// T4-08/FR-LOC-005: excluir um item já sincronizado — "manter remoto"
/// preserva o objeto do outro lado (sem exclusão remota) e cria uma cópia
/// local independente; "remover remoto" enfileira a exclusão de verdade.
#[tokio::test]
async fn excluding_an_already_synced_item_offers_keep_remote_or_remove_remote() {
    let (core, root, .., provider) = build_core().await;

    let keep_id = core.create_local_item(root, "manter.txt", ItemKind::File).await.unwrap();
    let keep_path = core.begin_write(keep_id).await.unwrap();
    tokio::fs::write(&keep_path, b"conteudo a manter").await.unwrap();
    core.stabilize_upload(keep_id).await.unwrap();

    let remove_id = core.create_local_item(root, "remover.txt", ItemKind::File).await.unwrap();
    core.begin_write(remove_id).await.unwrap();
    core.stabilize_upload(remove_id).await.unwrap();

    core.dispatch_pending_operations().await.unwrap();
    let keep_remote_id = core.get_item(keep_id).await.unwrap().unwrap().remote_item_id.unwrap();
    let remove_remote_id = core.get_item(remove_id).await.unwrap().unwrap().remote_item_id.unwrap();

    core.migrate_normal_sync_to_local_only(keep_id, true).await.unwrap();
    core.migrate_normal_sync_to_local_only(remove_id, false).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    let keep = core.get_item(keep_id).await.unwrap().unwrap();
    assert_eq!(keep.source_layer, "LOCAL_ONLY");
    assert!(keep.remote_item_id.is_none(), "cliente para de rastrear o vínculo mesmo mantendo o remoto");
    let hydrated = core.open_and_hydrate(keep_id).await.unwrap();
    assert_eq!(tokio::fs::read(&hydrated).await.unwrap(), b"conteudo a manter", "cópia local independente precisa ter os bytes reais");

    // "Manter remoto" preserva o objeto do outro lado intacto.
    let remote_kept = provider
        .get_item(nexofs_provider_api::GetItemRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            remote_item_id: nexofs_domain::RemoteItemId::from(keep_remote_id),
        })
        .await
        .unwrap();
    assert!(remote_kept.is_some(), "manter remoto não pode apagar o objeto do outro lado");

    let removed = core.get_item(remove_id).await.unwrap().unwrap();
    assert_eq!(removed.source_layer, "LOCAL_ONLY", "\"remover remoto\" ainda MANTÉM a cópia local — só o remoto some (SPEC §17.5)");
    assert!(removed.remote_item_id.is_none());
    let remote_removed = provider
        .get_item(nexofs_provider_api::GetItemRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            remote_item_id: nexofs_domain::RemoteItemId::from(remove_remote_id),
        })
        .await
        .unwrap();
    assert!(remote_removed.is_none(), "remover remoto precisa ter apagado de verdade");
}
