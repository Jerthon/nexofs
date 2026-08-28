//! T3-11/SPEC §10.3/§7.5 — teste de carga do scheduler: um backlog grande e
//! misto (upload, criação de diretório, exclusão, movimentação) deve ser
//! despachado em ordem estrita de prioridade (`OperationClass::default_priority`,
//! a mesma tabela usada pelo Governor), nunca por ordem de chegada crua.

use nexofs_api_governor::{OperationClass, ProviderApiGovernor};
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ItemKind, ProviderAccountContext};
use nexofs_provider_fake::{FakeProvider, RecordingProvider};
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

async fn build_core() -> (Arc<SyncCore>, nexofs_domain::ItemId, Arc<RecordingProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let dir = Box::leak(Box::new(dir));
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let fake = Arc::new(FakeProvider::new());
    let provider = Arc::new(RecordingProvider::new(fake.clone() as Arc<dyn CloudProvider>));
    // Limites de concorrência bem acima do padrão (SPEC §7.8 é um teto de
    // proteção contra o provedor real, não uma restrição a testar aqui) —
    // este teste mede ordenação de prioridade sob volume, não throttling
    // (já coberto em T2-04); com o limite padrão o próprio token bucket
    // introduziria dezenas de segundos de espera real de wall-clock.
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(OperationClass::Upload, 1000);
    overrides.insert(OperationClass::RemoteMutation, 1000);
    let governor = Arc::new(ProviderApiGovernor::with_concurrency_overrides(overrides));
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let namespaces = fake.list_namespaces(&account_ctx()).await.unwrap();
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
    (core, root, provider)
}

/// 30 uploads (`OperationClass::Upload`, prioridade 20) e 40 mutações
/// remotas (`OperationClass::RemoteMutation`, prioridade 50 — criação de
/// diretório, exclusão e renomeio) enfileiradas intercaladas, todas de
/// itens de nível superior (pai é a raiz, sem dependência a resolver) para
/// que uma única rodada do dispatcher as processe todas de uma vez, na
/// ordem exata que `due_operations` escolher.
#[tokio::test]
async fn a_large_mixed_backlog_dispatches_strictly_in_priority_order() {
    let (core, root, provider) = build_core().await;

    // Pré-existentes: sincronizados antes do backlog medido, para servirem
    // de alvo de exclusão/renomeio dentro do backlog em si.
    let mut to_delete = Vec::new();
    let mut to_rename = Vec::new();
    for i in 0..20 {
        let id = core.create_local_item(root, &format!("preexistente-del-{i}.txt"), ItemKind::File).await.unwrap();
        core.begin_write(id).await.unwrap();
        core.stabilize_upload(id).await.unwrap();
        to_delete.push((id, format!("preexistente-del-{i}.txt")));
    }
    for i in 0..20 {
        let id = core.create_local_item(root, &format!("preexistente-mv-{i}.txt"), ItemKind::File).await.unwrap();
        core.begin_write(id).await.unwrap();
        core.stabilize_upload(id).await.unwrap();
        to_rename.push((id, format!("preexistente-mv-{i}.txt")));
    }
    core.dispatch_pending_operations().await.unwrap();
    assert!(provider.calls().iter().all(|c| *c == "UPLOAD"), "pré-condição: só os 40 uploads iniciais até aqui");
    assert_eq!(provider.calls().len(), 40);

    // Backlog medido: intercala upload (prioridade 20) com mutação remota
    // (prioridade 50) na ordem de criação, para garantir que a saída não é
    // por acaso já ordenada por tipo.
    for i in 0..30 {
        let id = core.create_local_item(root, &format!("novo-{i}.txt"), ItemKind::File).await.unwrap();
        core.begin_write(id).await.unwrap();
        core.stabilize_upload(id).await.unwrap();

        if i < 20 {
            core.delete_local_item(root, &to_delete[i].1, ItemKind::File).await.unwrap();
        }
        if i < 10 {
            core.rename_local_item(root, &to_rename[i].1, root, &format!("renomeado-{i}.txt")).await.unwrap();
        }
        if i < 10 {
            core.create_local_item(root, &format!("pasta-nova-{i}"), ItemKind::Directory).await.unwrap();
        }
    }

    core.dispatch_pending_operations().await.unwrap();

    let calls = provider.calls();
    let measured = &calls[40..]; // descarta os 40 uploads da pré-condição
    assert_eq!(measured.len(), 30 + 20 + 10 + 10, "todas as operações do backlog medido devem ter sido despachadas numa única rodada");

    let first_mutation = measured.iter().position(|c| *c != "UPLOAD");
    let last_upload = measured.iter().rposition(|c| *c == "UPLOAD");
    assert!(
        first_mutation.is_none() || last_upload.is_none() || last_upload < first_mutation,
        "todo UPLOAD (prioridade 20) deve ser despachado antes de qualquer CREATE_DIRECTORY/DELETE/MOVE (prioridade 50): {measured:?}"
    );
    assert_eq!(measured.iter().filter(|c| **c == "UPLOAD").count(), 30);
    assert_eq!(measured.iter().filter(|c| **c == "DELETE").count(), 20);
    assert_eq!(measured.iter().filter(|c| **c == "MOVE").count(), 10);
    assert_eq!(measured.iter().filter(|c| **c == "CREATE_DIRECTORY").count(), 10);
}
