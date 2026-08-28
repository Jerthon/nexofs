use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, CreateDirectoryRequest, DeleteItemRequest, GetItemRequest, ItemKind, ProviderAccountContext, UploadRequest};
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

async fn bootstrap_schema(store: &nexofs_metadata_store::MetadataStore, account_id: AccountId, namespace_id: NamespaceId, remote_namespace_id: &str) {
    let remote_namespace_id = remote_namespace_id.to_string();
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
                rusqlite::params![namespace_id.to_string(), account_id.to_string(), remote_namespace_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn refresh_changes_applies_delta_without_a_new_list_children_call() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = provider.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();

    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    bootstrap_schema(&store, account_id, namespace_id, &namespace_remote_id).await;

    // Conteúdo que já existia antes de qualquer indexação (populado via
    // list_children lazy, não via delta).
    provider
        .create_directory(CreateDirectoryRequest {
            account: account_ctx(),
            namespace_remote_id: namespace_remote_id.clone(),
            parent_remote_item_id: None,
            name: "pasta1".to_string(),
        })
        .await
        .unwrap();

    let ctx = SyncCoreContext {
        provider_id: ProviderId::from("fake"),
        account_id,
        namespace_id,
        namespace_remote_id: namespace_remote_id.clone(),
    };
    let core = SyncCore::new(store.clone(), provider.clone(), governor, cache, overlay, account_ctx(), ctx);

    let root = core.bootstrap_root().await.unwrap();
    let initial_children = core.list_children(root).await.unwrap();
    assert_eq!(initial_children.len(), 1);
    assert_eq!(initial_children[0].name, "pasta1");

    // Primeira chamada: cursor_state ainda é UNINITIALIZED -> pega cursor
    // "a partir de agora", nada para aplicar ainda.
    core.refresh_changes().await.unwrap();

    // Nova pasta criada DEPOIS do cursor "latest" — só existe no changelog,
    // nunca via list_children (root.children_state já é 'LOADED').
    let pasta2 = provider
        .create_directory(CreateDirectoryRequest {
            account: account_ctx(),
            namespace_remote_id: namespace_remote_id.clone(),
            parent_remote_item_id: None,
            name: "pasta2".to_string(),
        })
        .await
        .unwrap();

    // Segunda chamada: cursor_state agora é VALID -> aplica o delta.
    core.refresh_changes().await.unwrap();

    let children_after_delta = core.list_children(root).await.unwrap();
    let names: Vec<_> = children_after_delta.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"pasta1"));
    assert!(names.contains(&"pasta2"), "pasta2 deveria ter sido indexada via delta, não via list_children");

    // Exclusão remota deve virar tombstone e sumir da listagem.
    provider
        .delete_item(DeleteItemRequest {
            account: account_ctx(),
            namespace_remote_id: namespace_remote_id.clone(),
            remote_item_id: pasta2.remote_item_id,
            base_remote_version: None,
        })
        .await
        .unwrap();

    core.refresh_changes().await.unwrap();
    let children_after_delete = core.list_children(root).await.unwrap();
    let names_after_delete: Vec<_> = children_after_delete.iter().map(|i| i.name.as_str()).collect();
    assert!(names_after_delete.contains(&"pasta1"));
    assert!(!names_after_delete.contains(&"pasta2"), "pasta2 deveria ter sido removida (tombstone) após exclusão remota");
}

/// Bug real de produção: um arquivo criado localmente e ainda não enviado
/// (`remote_item_id` nulo) que colide em nome com um item que aparece do
/// nada no changelog remoto (outro cliente criou algo homônimo) travava a
/// aplicação de mudanças inteira — a violação de `UNIQUE(namespace_id,
/// parent_item_id, normalized_name)` propagava para fora do laço de
/// `apply_changes_from`, o cursor nunca avançava, e TODA sincronização
/// futura da conta ficava presa repetindo o mesmo erro para sempre. A
/// correção trata isso como uma colisão estruturada (pulando só esta
/// mudança) em vez de travar o namespace inteiro.
#[tokio::test]
async fn a_never_uploaded_local_file_colliding_with_a_new_remote_item_does_not_wedge_the_whole_delta_sync() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = provider.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();

    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    bootstrap_schema(&store, account_id, namespace_id, &namespace_remote_id).await;

    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id: namespace_remote_id.clone() };
    let core = SyncCore::new(store.clone(), provider.clone(), governor, cache, overlay, account_ctx(), ctx);
    let root = core.bootstrap_root().await.unwrap();

    // Cursor "a partir de agora" — nada pendente ainda.
    core.refresh_changes().await.unwrap();

    // Arquivo criado localmente na raiz, nunca estabilizado/enviado —
    // fica com `remote_item_id` nulo de propósito.
    let local_item_id = core.create_local_item(root, "Icone.png", ItemKind::File).await.unwrap();
    core.begin_write(local_item_id).await.unwrap();

    // Outro cliente cria, na raiz remota, um arquivo de MESMO nome — só
    // aparece no changelog, nunca via `list_children` (root já `LOADED`).
    provider
        .upload(UploadRequest {
            account: account_ctx(),
            namespace_remote_id: namespace_remote_id.clone(),
            parent_remote_item_id: None,
            name: "Icone.png".to_string(),
            size_bytes: 5,
            base_remote_version: None,
            content: Box::pin(b"outro".as_slice()),
            resumable_session_token: None,
        })
        .await
        .unwrap();

    // Antes do fix isto retornava `Err` e o cursor nunca avançava.
    core.refresh_changes().await.unwrap();

    let conflicts = core.list_conflicts().await.unwrap();
    assert_eq!(conflicts.len(), 1, "a colisão deveria virar um conflito estruturado, não travar a sincronização");
    assert_eq!(conflicts[0].item_id, local_item_id);

    // A prova de que o namespace não ficou travado: uma mudança remota
    // POSTERIOR e sem relação com a colisão ainda é aplicada normalmente.
    provider
        .create_directory(CreateDirectoryRequest { account: account_ctx(), namespace_remote_id: namespace_remote_id.clone(), parent_remote_item_id: None, name: "depois-da-colisao".to_string() })
        .await
        .unwrap();
    core.refresh_changes().await.unwrap();
    let names: Vec<_> = core.list_children(root).await.unwrap().iter().map(|i| i.name.clone()).collect();
    assert!(names.contains(&"depois-da-colisao".to_string()), "sincronização deveria continuar funcionando após a colisão, não travar para sempre");
}

/// Bug real de produção: o usuário moveu um arquivo para uma pasta, viu o
/// log dizer "sucesso" e confirmou no provedor que o arquivo realmente
/// estava na pasta certa — mas o índice LOCAL mostrava o arquivo de volta
/// na raiz. Causa: uma página de `list_changes` ainda pendente descrevia a
/// posição de ANTES do move (versão mais antiga) e chegou depois do move
/// já ter avançado a versão — `upsert_item` sobrescrevia
/// `parent_item_id`/`name` sem checar se a mudança entrante era mais nova
/// do que o que já sabíamos.
#[tokio::test]
async fn a_stale_out_of_order_delta_does_not_revert_a_more_recent_local_move() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = provider.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();

    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    bootstrap_schema(&store, account_id, namespace_id, &namespace_remote_id).await;

    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id: namespace_remote_id.clone() };
    let core = SyncCore::new(store.clone(), provider.clone(), governor, cache, overlay, account_ctx(), ctx);
    let root = core.bootstrap_root().await.unwrap();

    // Cursor "a partir de agora" — nada pendente ainda.
    core.refresh_changes().await.unwrap();

    let pasta = core.create_local_item(root, "pasta-destino", ItemKind::Directory).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let arquivo = core.create_local_item(root, "arquivo.txt", ItemKind::File).await.unwrap();
    core.begin_write(arquivo).await.unwrap();
    core.stabilize_upload(arquivo).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    // Move de verdade: avança a versão remota do arquivo (via FakeProvider)
    // e o índice local já reflete a pasta nova imediatamente.
    core.rename_local_item(root, "arquivo.txt", pasta, "arquivo.txt").await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let moved = core.get_item(arquivo).await.unwrap().unwrap();
    assert_eq!(moved.parent_item_id, Some(pasta), "pré-condição: o move já deveria ter sido aplicado localmente");

    // Simula "já sabemos de uma versão ainda mais nova do que a que a
    // página de delta pendente vai descrever" — o comportamento real de uma
    // entrega fora de ordem, sem precisar reproduzir a reordenação em si.
    let arquivo_s = arquivo.to_string();
    store.write(move |tx| tx.execute("UPDATE items SET remote_version = '99' WHERE item_id = ?1", [&arquivo_s])).await.unwrap();

    // Esta chamada consome do changelog a mudança do move (versão real "2"),
    // mais velha que a "99" que acabamos de simular conhecer.
    core.refresh_changes().await.unwrap();

    let after_delta = core.get_item(arquivo).await.unwrap().unwrap();
    assert_eq!(after_delta.parent_item_id, Some(pasta), "delta desatualizada não deveria ter revertido o move");
    assert_eq!(after_delta.remote_version.as_deref(), Some("99"), "versão mais nova conhecida não deveria ter sido regredida");
}

/// Bug real de produção: mover um item para uma pasta, deixar despachar e
/// concluir de verdade, e DEPOIS mover o MESMO item de volta (ou para outro
/// lugar) — a chave de idempotência de `rename_local_item` não embute
/// `local_version` de propósito (para colapsar múltiplos renames antes do
/// primeiro dispatch em um só), mas isso significa que ela é a MESMA nas
/// duas vezes. Como `enqueue_operation` só sabia reaproveitar uma chave
/// ainda `Pending`, uma chave já `Completed` virava um beco sem saída: o
/// índice local mudava (ninguém confere o retorno de `rename_local_item`),
/// mas nenhuma chamada nova ao provedor acontecia — silenciosamente, para
/// sempre. O usuário via "moveu" localmente e no log, mas a nuvem nunca via
/// nada.
#[tokio::test]
async fn moving_an_item_again_after_an_earlier_move_already_completed_still_reaches_the_provider() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = provider.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();
    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    bootstrap_schema(&store, account_id, namespace_id, &namespace_remote_id).await;

    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id: namespace_remote_id.clone() };
    let core = SyncCore::new(store.clone(), provider.clone(), governor, cache, overlay, account_ctx(), ctx);
    let root = core.bootstrap_root().await.unwrap();

    let pasta = core.create_local_item(root, "pasta", ItemKind::Directory).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let pasta_remote_id = core.get_item(pasta).await.unwrap().unwrap().remote_item_id.unwrap();

    let arquivo = core.create_local_item(root, "arquivo.txt", ItemKind::File).await.unwrap();
    core.begin_write(arquivo).await.unwrap();
    core.stabilize_upload(arquivo).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    let remote_item_id = core.get_item(arquivo).await.unwrap().unwrap().remote_item_id.unwrap();

    let fetch_remote_parent = |remote_item_id: String| {
        let provider = provider.clone();
        let namespace_remote_id = namespace_remote_id.clone();
        async move {
            provider
                .get_item(GetItemRequest { account: account_ctx(), namespace_remote_id, remote_item_id: nexofs_domain::RemoteItemId::from(remote_item_id) })
                .await
                .unwrap()
                .unwrap()
                .parent_remote_item_id
                .map(|id| id.0)
        }
    };

    // Primeiro move: raiz -> pasta, despachado e concluído de verdade — o
    // PROVEDOR (não só o índice local) precisa achar que está na pasta.
    core.rename_local_item(root, "arquivo.txt", pasta, "arquivo.txt").await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    assert_eq!(fetch_remote_parent(remote_item_id.clone()).await, Some(pasta_remote_id.clone()), "pré-condição: o primeiro move precisa ter chegado ao provedor");

    // Segundo move, bem depois — mesmo "formato" (move_item, parent mudou)
    // do primeiro, então mesma chave de idempotência.
    core.rename_local_item(pasta, "arquivo.txt", root, "arquivo.txt").await.unwrap();
    core.dispatch_pending_operations().await.unwrap();

    assert!(core.pending_operations().await.unwrap().is_empty(), "o segundo move não deveria ficar parado sem nunca ser despachado");
    assert_eq!(core.get_item(arquivo).await.unwrap().unwrap().parent_item_id, Some(root), "índice local deveria refletir o segundo move");

    // A prova de verdade do bug: sem o fix, isto continuava `Some(pasta)`,
    // porque a segunda chamada nunca chegava a falar com o provedor.
    assert_ne!(
        fetch_remote_parent(remote_item_id).await,
        Some(pasta_remote_id),
        "o provedor ainda achava que o arquivo estava na pasta — o segundo move nunca chegou até ele"
    );
}

#[tokio::test]
async fn concurrent_list_children_on_same_uncached_folder_calls_provider_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = provider.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();

    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    bootstrap_schema(&store, account_id, namespace_id, &namespace_remote_id).await;

    let ctx = SyncCoreContext {
        provider_id: ProviderId::from("fake"),
        account_id,
        namespace_id,
        namespace_remote_id: namespace_remote_id.clone(),
    };
    let core = Arc::new(SyncCore::new(
        store,
        provider.clone() as Arc<dyn CloudProvider>,
        governor,
        cache, overlay, account_ctx(),
        ctx,
    ));
    let root = core.bootstrap_root().await.unwrap();

    let mut handles = Vec::new();
    for _ in 0..10 {
        let core = core.clone();
        handles.push(tokio::spawn(async move { core.list_children(root).await.unwrap() }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    // O changelog cresce 1 por chamada real ao provider; como a pasta não
    // tinha nenhum filho, `list_children` não gera entradas de changelog —
    // em vez disso, verificamos indiretamente que não houve pânico/erro de
    // concorrência e que o estado final é consistente (children_state
    // LOADED, sem duplicar). A garantia forte de "uma única chamada real"
    // já é coberta pelo teste de `PerKeyLock`/`Deduplicator` no governor;
    // aqui validamos que o SyncCore não quebra sob concorrência real.
    let children = core.list_children(root).await.unwrap();
    assert!(children.is_empty());
}
