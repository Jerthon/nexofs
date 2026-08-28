//! T4-09/SPEC §7.9/API-020 — controle de tempestade de arquivos: mais de
//! 1000 novos itens em 30s numa mesma pasta pausa a criação de operações
//! remotas para ela, sem perder o conteúdo local; a retomada é sempre
//! explícita.

use nexofs_api_governor::{OperationClass, ProviderApiGovernor};
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ItemKind, ProviderAccountContext};
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

async fn build_core() -> (Arc<SyncCore>, nexofs_domain::ItemId) {
    let dir = tempfile::tempdir().unwrap();
    let dir = Box::leak(Box::new(dir));
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    // Este teste mede o gate de tempestade, não throttling (já coberto em
    // T2-04) — com o limite padrão de concorrência, despachar 1000+
    // operações levaria minutos reais de wall-clock só no token bucket.
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(OperationClass::RemoteMutation, 2000);
    let governor = Arc::new(ProviderApiGovernor::with_concurrency_overrides(overrides));
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
    let core = Arc::new(SyncCore::new(store, provider, governor, cache, overlay, account_ctx(), ctx));
    let root = core.bootstrap_root().await.unwrap();
    (core, root)
}

#[tokio::test]
async fn creating_over_1000_directories_in_the_same_folder_pauses_new_remote_operations_for_it() {
    let (core, root) = build_core().await;
    let burst_dir = core.create_local_item(root, "burst", ItemKind::Directory).await.unwrap();
    core.dispatch_pending_operations().await.unwrap();
    assert!(core.get_item(burst_dir).await.unwrap().unwrap().remote_item_id.is_some(), "pré-condição: a própria pasta do teste já sincronizada");

    let mut created = Vec::new();
    for i in 0..1005 {
        created.push(core.create_local_item(burst_dir, &format!("item-{i}"), ItemKind::Directory).await.unwrap());
    }

    assert_eq!(core.storm_paused_folders(), vec![burst_dir], "mais de 1000 criações em 30s deve pausar a pasta");

    core.dispatch_pending_operations().await.unwrap();
    for id in &created {
        assert!(
            core.get_item(*id).await.unwrap().unwrap().remote_item_id.is_none(),
            "nada deveria ter sido criado remotamente enquanto a pasta está pausada — a operação continua no journal, só o dispatcher recusa executá-la"
        );
    }
    let pending = core.pending_operations().await.unwrap();
    assert!(
        created.iter().all(|id| pending.iter().any(|op| op.item_id == Some(*id))),
        "a intenção de sincronizar não pode ter se perdido — cada item continua Pending no journal, só não despachado"
    );

    // Retomada explícita libera tudo de uma vez.
    core.resume_from_storm_pause(burst_dir).await.unwrap();
    assert!(core.storm_paused_folders().is_empty());

    core.dispatch_pending_operations().await.unwrap();
    let mut synced = 0;
    for id in &created {
        if core.get_item(*id).await.unwrap().unwrap().remote_item_id.is_some() {
            synced += 1;
        }
    }
    assert_eq!(synced, created.len(), "depois da retomada, todos os itens da rajada devem sincronizar normalmente");
}
