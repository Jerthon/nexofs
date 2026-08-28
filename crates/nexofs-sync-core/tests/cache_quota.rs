use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ItemKind, ProviderAccountContext, UploadRequest};
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
async fn evicts_oldest_hydrated_item_when_over_quota_but_keeps_open_ones() {
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

    for (name, content) in [("a.txt", b"11111".to_vec()), ("b.txt", b"22222".to_vec()), ("c.txt", b"33333".to_vec())] {
        provider
            .upload(UploadRequest {
                account: account_ctx(),
                namespace_remote_id: namespace_remote_id.clone(),
                parent_remote_item_id: None,
                name: name.to_string(),
                size_bytes: content.len() as u64,
                base_remote_version: None,
                content: Box::pin(std::io::Cursor::new(content)),
                resumable_session_token: None,
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
    let mut children = core.list_children(root).await.unwrap();
    children.sort_by(|a, b| a.name.cmp(&b.name));
    let (a, b, c) = (children[0].item_id, children[1].item_id, children[2].item_id);

    // Hidrata os três (5 bytes cada = 15 bytes no total), na ordem a, b, c
    // — 'a' é o menos recentemente acessado.
    core.open_and_hydrate(a).await.unwrap();
    core.open_and_hydrate(b).await.unwrap();
    // 'c' fica com um handle aberto — nunca pode ser evictado mesmo sendo o
    // mais recente ou não.
    core.mark_handle_opened(c).await.unwrap();
    core.open_and_hydrate(c).await.unwrap();

    // Quota de 10 bytes força evictar 5 bytes — só 'a' é elegível por ser o
    // mais antigo entre os sem handle aberto ('b' tem 5 bytes livres já
    // bastam, mas 'a' é escolhido primeiro por ordem de acesso).
    core.enforce_cache_quota(10).await.unwrap();

    assert_eq!(
        core.hydration_state_of(a).await.unwrap().as_deref(),
        Some("EVICTED"),
        "'a' deveria ter sido evictado (mais antigo, sem handle aberto)"
    );
    assert_eq!(
        core.hydration_state_of(c).await.unwrap().as_deref(),
        Some("HYDRATED"),
        "'c' nunca pode ser evictado com handle aberto"
    );
}

/// T4-11/SPEC §12.5: um item `Dirty` (edição local não enviada) ou
/// `LocalOnly` também tem `hydration_state = 'HYDRATED'` — a query de
/// elegibilidade de antes só olhava para isso e `open_handle_count`, então
/// sob pressão de cache um arquivo com edição pendente podia ser marcado
/// `EVICTED` mesmo com o conteúdo real intacto em `dirty/`/no overlay,
/// corrompendo o tamanho que `getattr` reportaria depois (a promoção real
/// para `clean/` nunca acontecia, então nada era de fato apagado do disco —
/// só o rastreamento em `local_states` ficava incoerente).
#[tokio::test]
async fn dirty_and_local_only_items_are_never_evicted_even_under_quota_pressure() {
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
    let core = SyncCore::new(store, provider, governor, cache, overlay, account_ctx(), ctx);
    let root = core.bootstrap_root().await.unwrap();

    let dirty_id = core.create_local_item(root, "editando.txt", ItemKind::File).await.unwrap();
    let dirty_path = core.begin_write(dirty_id).await.unwrap();
    tokio::fs::write(&dirty_path, b"1234567890").await.unwrap();
    core.update_local_size(dirty_id, 10).await.unwrap();

    core.add_ignore_rule(nexofs_sync_core::RuleTier::UserGlobal, "*.tmp").await.unwrap();
    let local_only_id = core.create_local_item(root, "temp.tmp", ItemKind::File).await.unwrap();
    let overlay_path = core.begin_write(local_only_id).await.unwrap();
    tokio::fs::write(&overlay_path, b"1234567890").await.unwrap();
    core.update_local_size(local_only_id, 10).await.unwrap();

    // Quota impossivelmente pequena — se a elegibilidade estivesse errada,
    // isso evictaria os dois itens acima (ambos `hydration_state =
    // 'HYDRATED'`, sem handle aberto).
    core.enforce_cache_quota(0).await.unwrap();

    assert_eq!(core.hydration_state_of(dirty_id).await.unwrap().as_deref(), Some("HYDRATED"), "item Dirty não pode ser evictado");
    assert_eq!(core.hydration_state_of(local_only_id).await.unwrap().as_deref(), Some("HYDRATED"), "item LocalOnly não pode ser evictado");
    assert!(dirty_path.is_file(), "conteúdo dirty real precisa continuar íntegro em disco");
    assert!(overlay_path.is_file(), "conteúdo do overlay precisa continuar íntegro em disco");
}
