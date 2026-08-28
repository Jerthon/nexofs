use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{
    CloudProvider, CreateDirectoryRequest, ItemKind, ProviderAccountContext, UploadRequest,
};
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

#[tokio::test]
async fn lists_lazily_and_hydrates_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = provider.list_namespaces(&account_ctx()).await.unwrap();
    let namespace_remote_id = namespaces[0].remote_namespace_id.clone();

    // Popula o provider ANTES de qualquer chamada do SyncCore — simula
    // conteúdo que já existia na nuvem antes da primeira navegação local.
    provider
        .create_directory(CreateDirectoryRequest {
            account: account_ctx(),
            namespace_remote_id: namespace_remote_id.clone(),
            parent_remote_item_id: None,
            name: "pasta".to_string(),
        })
        .await
        .unwrap();
    provider
        .upload(UploadRequest {
            account: account_ctx(),
            namespace_remote_id: namespace_remote_id.clone(),
            parent_remote_item_id: None,
            name: "arquivo.txt".to_string(),
            size_bytes: 5,
            base_remote_version: None,
            content: Box::pin(b"hello".as_slice()),
            resumable_session_token: None,
        })
        .await
        .unwrap();

    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();

    // O schema exige providers -> accounts -> namespaces em cascata (FKs);
    // em produção isso é criado pelo fluxo de "adicionar conta" (Fase 1,
    // nexofsd). Aqui simulamos apenas o bootstrap mínimo necessário.
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
    let core = SyncCore::new(store, provider, governor, cache, overlay, account_ctx(), ctx);

    let root = core.bootstrap_root().await.unwrap();

    // FR-IDX-002/003: a raiz já existe antes de qualquer listagem ampla, e
    // os filhos só são buscados quando a pasta é efetivamente acessada.
    let children_first_call = core.list_children(root).await.unwrap();
    assert_eq!(children_first_call.len(), 2);

    // Segunda chamada não deve mudar o resultado (atendida pelo índice
    // local — children_state já está 'LOADED').
    let children_second_call = core.list_children(root).await.unwrap();
    assert_eq!(children_second_call.len(), 2);

    let file_item = children_second_call
        .iter()
        .find(|i| i.kind == ItemKind::File)
        .expect("arquivo.txt deve estar entre os filhos");
    assert_eq!(file_item.name, "arquivo.txt");

    let dir_item = children_second_call
        .iter()
        .find(|i| i.kind == ItemKind::Directory)
        .expect("pasta deve estar entre os filhos");
    assert_eq!(dir_item.name, "pasta");

    // FR-HYD-001/002: abrir hidrata via arquivo temporário + promoção atômica.
    let hydrated_path = core.open_and_hydrate(file_item.item_id).await.unwrap();
    assert_eq!(tokio::fs::read(&hydrated_path).await.unwrap(), b"hello");

    // Reabrir não deve exigir nova rede — mesmo caminho, sem erro.
    let hydrated_again = core.open_and_hydrate(file_item.item_id).await.unwrap();
    assert_eq!(hydrated_path, hydrated_again);

    // lookup_child também deve resolver pelo índice local.
    let looked_up = core.lookup_child(root, "ARQUIVO.TXT").await.unwrap();
    assert!(looked_up.is_some(), "lookup deve ser case-insensitive-preserving");
}
