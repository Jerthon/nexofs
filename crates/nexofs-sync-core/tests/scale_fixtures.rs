//! T6-01/T6-02 (Fase 6, "Escala e hardening"): fixture sintética de itens
//! num único diretório + medição de que `list_children` continua rápido
//! (consulta indexada por `parent_item_id`, `idx_items_parent` do schema)
//! quando o diretório já foi listado antes (`children_state = 'LOADED'`,
//! caminho "conteúdo já conhecido localmente" — o caso comum de reabrir uma
//! pasta grande já visitada).
//!
//! Deliberadamente NÃO passa pelo caminho real de descoberta
//! (`ensure_children_loaded`, que faz uma transação SQLite por item
//! upsertado) — construir a fixture em si usa inserção em lote (poucas
//! transações grandes), porque o objetivo aqui é medir o custo de LER um
//! diretório já indexado, não o custo de indexá-lo pela primeira vez (que é
//! dominado pela paginação do provedor remoto, uma preocupação diferente).
//!
//! 1 milhão/5 milhões de itens (os outros dois tamanhos pedidos por T6-01)
//! não rodam por padrão aqui — ver `fixture_at_scale` e o comentário em
//! `huge_directories_stay_fast_at_larger_scales_too` (`#[ignore]`, rode com
//! `cargo test --test scale_fixtures -- --ignored --nocapture` quando quiser
//! medir de verdade; não é executado automaticamente porque pode levar
//! minutos e não é isso que a suíte de testes normal deve pagar a cada
//! `cargo test`).

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_domain::{AccountId, ItemId, NamespaceId, ProviderId};
use nexofs_provider_api::CloudProvider;
use nexofs_provider_fake::FakeProvider;
use nexofs_sync_core::{SyncCore, SyncCoreContext};
use rusqlite::params;
use std::sync::Arc;
use std::time::Instant;

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Monta um `SyncCore` com uma pasta já contendo `count` arquivos filhos
/// diretos, inseridos em lote (uma única transação) — a "ferramenta de
/// geração de fixtures" de T6-01. Retorna o núcleo e o `ItemId` da pasta
/// povoada, pronta para medir `list_children` nela.
async fn build_fixture(count: usize) -> (Arc<SyncCore>, ItemId) {
    let dir = tempfile::tempdir().unwrap();
    let dir = Box::leak(Box::new(dir));
    let store = Arc::new(nexofs_metadata_store::MetadataStore::open(dir.path().join("nexofs.sqlite3")).unwrap());
    let provider: Arc<dyn CloudProvider> = Arc::new(FakeProvider::new());
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache = ContentCache::new(dir.path().join("cache/clean"), dir.path().join("cache/partial"), dir.path().join("cache/dirty"));
    let overlay = nexofs_overlay::LocalOnlyOverlay::new(dir.path().join("overlay"));

    let account_id = AccountId::new();
    let namespace_id = NamespaceId::new();
    let now = now_unix();
    store
        .write(move |tx| {
            tx.execute("INSERT INTO providers (provider_id, display_name, capabilities_json, created_at, updated_at) VALUES ('fake', 'Fake', '{}', 0, 0)", [])?;
            tx.execute(
                "INSERT INTO accounts (account_id, provider_id, provider_account_id, account_type, display_name, auth_state, created_at, updated_at) VALUES (?1, 'fake', 'fake-account', 'PERSONAL', 'Conta Fake', 'VALID', ?2, ?2)",
                params![account_id.to_string(), now],
            )?;
            tx.execute(
                "INSERT INTO namespaces (namespace_id, account_id, remote_namespace_id, display_name, namespace_type, mount_path, mount_state, created_at, updated_at) VALUES (?1, ?2, 'fake-namespace', 'Fake', 'PERSONAL', '/tmp/fake-mount', 'MOUNTED', ?3, ?3)",
                params![namespace_id.to_string(), account_id.to_string(), now],
            )
        })
        .await
        .unwrap();

    let ctx = SyncCoreContext { provider_id: ProviderId::from("fake"), account_id, namespace_id, namespace_remote_id: "fake-namespace".to_string() };
    let core = Arc::new(SyncCore::new(store.clone(), provider, governor, cache, overlay, account_ctx(), ctx));
    let root = core.bootstrap_root().await.unwrap();

    // Inserção em lote: a pasta em si, já marcada `LOADED` (não tem seus
    // próprios filhos ainda listados), e depois `count` arquivos filhos
    // numa única transação — é isso que faz a geração da fixture ser
    // rápida mesmo para milhões de itens, ao contrário de um upsert por
    // item via `ensure_children_loaded`.
    let folder_id = ItemId::new();
    let folder_id_s = folder_id.to_string();
    let namespace_id_s = namespace_id.to_string();
    let root_s = root.to_string();
    let now = now_unix();
    store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO items (item_id, namespace_id, remote_item_id, parent_item_id, name, normalized_name, item_type, size_bytes, children_state, remote_state, source_layer, created_at, updated_at) \
                 VALUES (?1, ?2, 'remote-folder', ?3, 'pasta-grande', 'pasta-grande', 'DIRECTORY', 0, 'UNKNOWN', 'PRESENT', 'REMOTE', ?4, ?4)",
                params![folder_id_s, namespace_id_s, root_s, now],
            )?;
            for i in 0..count {
                let item_id = uuid::Uuid::new_v4().to_string();
                let remote_id = format!("remote-item-{i}");
                let name = format!("arquivo-{i:07}.txt");
                tx.execute(
                    "INSERT INTO items (item_id, namespace_id, remote_item_id, parent_item_id, name, normalized_name, item_type, size_bytes, children_state, remote_state, source_layer, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5, 'FILE', 1024, 'UNKNOWN', 'PRESENT', 'REMOTE', ?6, ?6)",
                    params![item_id, namespace_id_s, remote_id, folder_id_s, name, now],
                )?;
            }
            // A pasta já está "carregada" — `list_children` nela não deve
            // tentar buscar nada do provedor fake (que nem sabe desses itens).
            tx.execute("UPDATE items SET children_state = 'LOADED' WHERE item_id = ?1", [&folder_id_s])?;
            Ok(())
        })
        .await
        .unwrap();

    (core, folder_id)
}

fn account_ctx() -> nexofs_provider_api::ProviderAccountContext {
    nexofs_provider_api::ProviderAccountContext {
        account_id: AccountId::new(),
        provider_account_id: "fake-account".to_string(),
        tenant_id: None,
        access_token: nexofs_domain::SecretToken::new("token"),
    }
}

#[tokio::test]
async fn a_folder_with_100_thousand_children_lists_quickly_once_already_indexed() {
    let (core, folder_id) = build_fixture(100_000).await;

    let started = Instant::now();
    let children = core.list_children(folder_id).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(children.len(), 100_000);
    // T6-02/PRD §15.2: não é um número oficial de SLA (a SPEC não define
    // uma "estação de referência") — é uma bacia de segurança generosa para
    // pegar uma regressão real (ex.: alguém trocando a consulta indexada
    // por uma varredura completa da tabela), não um benchmark de precisão.
    // Medido nesta máquina: ver `NexoFS_TASKS_v1.0.md` para o número real.
    assert!(elapsed.as_secs() < 10, "list_children levou {elapsed:?} para 100 mil itens já indexados — suspeita de regressão de performance");
    eprintln!("list_children (100.000 itens já indexados): {elapsed:?}");

    // Uma segunda chamada (o que `readdir` faria em cada chunk sem o cache
    // de `fh` da correção de T6-02 em `nexofs-fuse`) custa o mesmo — a
    // proteção real contra pagar isso repetidas vezes por sessão de
    // diretório aberta mora na camada FUSE, não aqui; este teste só prova
    // que o custo de UMA chamada é aceitável.
    let started = Instant::now();
    let children_again = core.list_children(folder_id).await.unwrap();
    let elapsed_again = started.elapsed();
    assert_eq!(children_again.len(), 100_000);
    eprintln!("list_children (segunda chamada, mesmo diretório): {elapsed_again:?}");
}

/// T6-01: a mesma ferramenta de fixture cobre 1 milhão e 5 milhões de itens
/// — não roda por padrão (`cargo test` normal) porque pode levar minutos;
/// `cargo test --test scale_fixtures -- --ignored --nocapture` mede de
/// verdade quando necessário.
#[tokio::test]
#[ignore]
async fn huge_directories_stay_fast_at_larger_scales_too() {
    for count in [1_000_000usize, 5_000_000usize] {
        let (core, folder_id) = build_fixture(count).await;
        let started = Instant::now();
        let children = core.list_children(folder_id).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(children.len(), count);
        eprintln!("list_children ({count} itens já indexados): {elapsed:?}");
    }
}
