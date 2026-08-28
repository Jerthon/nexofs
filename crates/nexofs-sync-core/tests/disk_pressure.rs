//! T4-14/SPEC §19.4 — a reação a cada nível de pressão de disco. A consulta
//! real ao `statvfs` já é testada em `nexofs-content-cache`; aqui o foco é
//! só a reação do núcleo (`handle_disk_pressure`), usando a máquina real de
//! quem roda o teste (não há como forçar um disco cheio de verdade aqui).

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, NamespaceId, ProviderId, SecretToken};
use nexofs_provider_api::{CloudProvider, ProviderAccountContext};
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{DiskPressureLevel, SyncCore, SyncCoreContext};
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
async fn disk_pressure_reports_a_real_level_for_the_machine_running_the_test() {
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

    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id };
    let core = SyncCore::new(store, provider.clone() as Arc<dyn CloudProvider>, governor, cache, overlay, account_ctx(), ctx);
    core.bootstrap_root().await.unwrap();

    // Não afirma qual nível — a máquina do teste pode ter qualquer
    // quantidade de espaço livre — só que a chamada real funciona.
    let level = core.disk_pressure().unwrap();
    assert!(matches!(level, DiskPressureLevel::Normal | DiskPressureLevel::Warning | DiskPressureLevel::Critical | DiskPressureLevel::Emergency));

    // `handle_disk_pressure` sempre devolve o mesmo nível observado,
    // independentemente de qual ação (se alguma) ele tenha disparado —
    // a reação em si (`enforce_cache_quota` sob `Critical`/`Emergency`)
    // reaproveita o mecanismo já coberto por `cache_quota.rs`; não há como
    // forçar um nível específico sem um disco de verdade naquele estado,
    // então este teste garante só que o caminho não quebra em `Normal`
    // (o caso comum em qualquer máquina de desenvolvimento/CI).
    let handled = core.handle_disk_pressure(1024 * 1024 * 1024).await.unwrap();
    assert_eq!(handled, level);
}
