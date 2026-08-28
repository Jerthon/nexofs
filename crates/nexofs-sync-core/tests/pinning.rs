//! T4-10/T4-11 — fixação (`PinState`) e a garantia de que um item fixado
//! nunca é evictado, mesmo sob pressão de cache.

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::states::PinState;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ProviderAccountContext, UploadRequest};
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

async fn build_core() -> (Arc<SyncCore>, nexofs_domain::ItemId, Arc<FakeProvider>) {
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
    let core = Arc::new(SyncCore::new(store, provider.clone() as Arc<dyn CloudProvider>, governor, cache, overlay, account_ctx(), ctx));
    let root = core.bootstrap_root().await.unwrap();
    (core, root, provider)
}

#[tokio::test]
async fn set_pin_state_persists_and_reports_back_correctly() {
    let (core, root, provider) = build_core().await;
    provider
        .upload(UploadRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            parent_remote_item_id: None,
            name: "a.txt".to_string(),
            size_bytes: 1,
            base_remote_version: None,
            content: Box::pin(b"x".as_slice()),
            resumable_session_token: None,
        })
        .await
        .unwrap();
    let item = core.list_children(root).await.unwrap().into_iter().next().unwrap();

    assert_eq!(core.pin_state_of(item.item_id).await.unwrap(), PinState::OnlineOnly, "estado padrão antes de qualquer fixação");

    core.set_pin_state(item.item_id, PinState::Pinned).await.unwrap();
    assert_eq!(core.pin_state_of(item.item_id).await.unwrap(), PinState::Pinned);

    core.set_pin_state(item.item_id, PinState::AvailableLocally).await.unwrap();
    assert_eq!(core.pin_state_of(item.item_id).await.unwrap(), PinState::AvailableLocally);
}

#[tokio::test]
async fn a_pinned_item_is_never_evicted_under_quota_pressure() {
    let (core, root, provider) = build_core().await;
    for (name, content) in [("a.txt", b"11111".to_vec()), ("b.txt", b"22222".to_vec())] {
        provider
            .upload(UploadRequest {
                account: account_ctx(),
                namespace_remote_id: "fake-namespace".to_string(),
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

    let mut children = core.list_children(root).await.unwrap();
    children.sort_by(|a, b| a.name.cmp(&b.name));
    let (a, b) = (children[0].item_id, children[1].item_id);

    core.open_and_hydrate(a).await.unwrap();
    core.open_and_hydrate(b).await.unwrap();
    core.set_pin_state(a, PinState::Pinned).await.unwrap();

    // Quota impossivelmente pequena — sem a proteção de T4-11, evictaria os
    // dois (ambos hidratados, sem handle aberto, mesmo tempo de acesso).
    core.enforce_cache_quota(0).await.unwrap();

    assert_eq!(core.hydration_state_of(a).await.unwrap().as_deref(), Some("HYDRATED"), "item fixado nunca pode ser evictado");
    assert_eq!(core.hydration_state_of(b).await.unwrap().as_deref(), Some("EVICTED"), "item não fixado continua elegível normalmente");
}

#[tokio::test]
async fn pin_recursive_marks_the_whole_subtree_and_hydrates_files_in_the_background() {
    let (core, root, provider) = build_core().await;
    provider
        .create_directory(nexofs_provider_api::CreateDirectoryRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            parent_remote_item_id: None,
            name: "pasta".to_string(),
        })
        .await
        .unwrap();
    let dir_item = core.list_children(root).await.unwrap().into_iter().next().unwrap();
    let dir_remote_id = dir_item.remote_item_id.clone().unwrap();

    provider
        .upload(UploadRequest {
            account: account_ctx(),
            namespace_remote_id: "fake-namespace".to_string(),
            parent_remote_item_id: Some(nexofs_domain::RemoteItemId::from(dir_remote_id)),
            name: "dentro.txt".to_string(),
            size_bytes: 3,
            base_remote_version: None,
            content: Box::pin(b"abc".as_slice()),
            resumable_session_token: None,
        })
        .await
        .unwrap();

    core.pin_recursive(dir_item.item_id);

    // A fixação e a hidratação em segundo plano são assíncronas — espera em
    // pequenos incrementos até o efeito aparecer, em vez de um sleep fixo.
    let file_item_id = loop {
        let children = core.list_children(dir_item.item_id).await.unwrap();
        if let Some(child) = children.first() {
            if core.pin_state_of(child.item_id).await.unwrap() == PinState::Pinned {
                break child.item_id;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };

    assert_eq!(core.pin_state_of(dir_item.item_id).await.unwrap(), PinState::Pinned);

    let mut waited = 0;
    loop {
        if core.hydration_state_of(file_item_id).await.unwrap().as_deref() == Some("HYDRATED") {
            break;
        }
        waited += 1;
        assert!(waited < 200, "descendente fixado deveria ter sido hidratado em segundo plano");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
