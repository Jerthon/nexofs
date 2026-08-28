//! Orquestração entre índice local, Governor, provider e cache de conteúdo.
//! Cobre indexação progressiva (FR-IDX-002/003), hidratação sob demanda
//! (FR-HYD-001/002/003) e sincronização incremental (FR-IDX-004/006,
//! SPEC §14).

mod conflict_resolution;
mod conflicts;
pub use conflict_resolution::ConflictSummary;
mod connectivity;
mod diagnostics;
mod disk_pressure;
mod dispatch;
pub use diagnostics::NamespaceDiagnostics;
mod ignore;
mod journal;
mod pin;
mod storm;
mod write;

use crate::error::SyncError;
use crate::model::{item_kind_to_sql, CacheBreakdown, CacheStats, IndexedItem};
use crate::queries::{now_unix, parse_item_id, row_to_item, FROM_ITEMS, ITEM_COLUMNS, NOT_DELETED_LOCALLY};
use crate::per_key_lock::PerKeyLock;
use nexofs_api_governor::{OperationClass, ProviderApiGovernor, RateScope};
use nexofs_content_cache::{CacheError, ContentCache};
use nexofs_overlay::LocalOnlyOverlay;
use nexofs_domain::{AccountId, ItemId, NamespaceId, ProviderId, RemoteItemId};
use nexofs_metadata_store::MetadataStore;
use nexofs_provider_api::{
    ChangeCursor, CloudProvider, CreateCursorRequest, DownloadRequest, ItemKind,
    ListChangesRequest, ListChildrenRequest, RemoteChange, RemoteItem,
};
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OnceCell, RwLock};

/// FR-ACT-006: "30 s como intervalo mínimo inicial" — nenhuma atividade,
/// por mais intensa, dispara mais de um refresh por namespace nessa janela.
const ACTIVITY_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// Extrai o sufixo numérico comum a todo formato de `remote_version`
/// observado até hoje — `"N"` do `FakeProvider`, `"{GUID},N"` do Graph — para
/// comparar "isto é mais velho do que o que já sei" sem acoplar
/// `nexofs-sync-core` ao formato de nenhum provedor específico. `None`
/// quando o formato não bate com esse padrão (ex.: um provedor futuro sem
/// sufixo numérico) — quem chama trata isso como "não dá pra comparar,
/// aplica normalmente" em vez de recusar a atualização.
fn version_ordinal(version: Option<&str>) -> Option<i64> {
    version?.rsplit(',').next()?.trim().parse::<i64>().ok()
}

#[derive(Debug, Clone)]
pub struct SyncCoreContext {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub namespace_id: NamespaceId,
    pub namespace_remote_id: String,
}

pub struct SyncCore {
    store: Arc<MetadataStore>,
    provider: Arc<dyn CloudProvider>,
    governor: Arc<ProviderApiGovernor>,
    cache: ContentCache,
    // T4-05/SPEC §11.4: armazenamento persistente para conteúdo `LocalOnly`
    // — nunca cache, nunca evictado, nunca gera operação remota.
    overlay: LocalOnlyOverlay,
    // T4-01/02: motor de avaliação de exclusão compilado, cacheado — ver
    // `core/ignore.rs`. `None` até a primeira avaliação; invalidado sempre
    // que uma regra é adicionada/removida.
    ignore_engine_cache: tokio::sync::RwLock<Option<Arc<nexofs_ignore::IgnoreEngine>>>,
    account_ctx: RwLock<nexofs_provider_api::ProviderAccountContext>,
    ctx: SyncCoreContext,
    root_item_id: OnceCell<ItemId>,
    loading_locks: PerKeyLock<ItemId>,
    hydrating_locks: PerKeyLock<ItemId>,
    // T1-08/T2-07: sessão de pasta ativa em memória + debounce por drive
    // (não por pasta — SPEC §9.4 "aplicar debounce e intervalo mínimo por
    // drive, não por pasta").
    active_directories: Mutex<HashMap<ItemId, Instant>>,
    last_activity_refresh_at: Mutex<Option<Instant>>,
    // T3-09/FR-OFF-005: sinal explícito de conectividade, atualizado por
    // toda chamada ao provedor que passa por `execute_governed` — ver
    // `core/connectivity.rs`. Otimista por padrão (`true`): o primeiro erro
    // de rede real já corrige para `false` em vez de esperar um estado
    // inicial pessimista sem necessidade.
    online: std::sync::atomic::AtomicBool,
    // T3-04/SPEC §16.2 ("5 segundos sem nova escrita"): uma tarefa agendada
    // por item dirty, reagendada (abortando a anterior) a cada novo `write`
    // — só dispara `stabilize_upload` se nenhuma escrita nova chegar dentro
    // da janela. Ver `core/write.rs::schedule_write_idle_stabilization`.
    write_idle_debounce: Mutex<HashMap<ItemId, tokio::task::JoinHandle<()>>>,
    // T4-09/SPEC §7.9: taxa de criação por pasta-mãe + conjunto de pastas
    // hoje pausadas por tempestade — ver `core/storm.rs`.
    storm_tracker: Mutex<storm::StormTracker>,
    // `nexofs-fuse` chama `mark_directory_active` de uma thread nativa do
    // fuser, fora de qualquer runtime Tokio — `tokio::spawn` (a função
    // livre) exigiria estar já dentro de um runtime e entraria em pânico
    // ali, derrubando a sessão FUSE inteira. Um `Handle` capturado no
    // momento da construção (sempre dentro de um runtime, seja no
    // `#[tokio::main]` do daemon ou em `#[tokio::test]`) pode ser usado a
    // partir de qualquer thread.
    runtime_handle: tokio::runtime::Handle,
    // T5-02/SPEC §20.4: cada instância nasce com um barramento próprio (sem
    // custo real — testes/uso avulso nunca assinam nada); `nexofsd` troca
    // por um único barramento compartilhado entre namespaces via
    // `with_event_bus`, para expor um `GET /v1/events` só, não um por conta.
    event_bus: Arc<crate::events::EventBus>,
    // T4-14/SPEC §19.4 item 5: último nível observado, só para não emitir
    // `CachePressureChanged` a cada tick de 30s quando nada mudou.
    last_disk_pressure: Mutex<nexofs_content_cache::DiskPressureLevel>,
}

impl SyncCore {
    /// Deve ser construído de dentro de um runtime Tokio ativo (`#[tokio::main]`
    /// ou `#[tokio::test]`) — ver nota em `runtime_handle`.
    pub fn new(
        store: Arc<MetadataStore>,
        provider: Arc<dyn CloudProvider>,
        governor: Arc<ProviderApiGovernor>,
        cache: ContentCache,
        overlay: LocalOnlyOverlay,
        account_ctx: nexofs_provider_api::ProviderAccountContext,
        ctx: SyncCoreContext,
    ) -> Self {
        Self {
            store,
            provider,
            governor,
            cache,
            overlay,
            ignore_engine_cache: tokio::sync::RwLock::new(None),
            account_ctx: RwLock::new(account_ctx),
            ctx,
            root_item_id: OnceCell::new(),
            loading_locks: PerKeyLock::new(),
            hydrating_locks: PerKeyLock::new(),
            active_directories: Mutex::new(HashMap::new()),
            last_activity_refresh_at: Mutex::new(None),
            online: std::sync::atomic::AtomicBool::new(true),
            write_idle_debounce: Mutex::new(HashMap::new()),
            storm_tracker: Mutex::new(storm::StormTracker::default()),
            runtime_handle: tokio::runtime::Handle::current(),
            event_bus: Arc::new(crate::events::EventBus::new()),
            last_disk_pressure: Mutex::new(nexofs_content_cache::DiskPressureLevel::Normal),
        }
    }

    /// T5-02: substitui o barramento individual por um compartilhado entre
    /// vários namespaces — chamado por `nexofsd` antes de montar o FUSE,
    /// nunca depois (assinantes que já tenham se registrado no barramento
    /// antigo, se algum, perderiam eventos publicados após a troca).
    pub fn with_event_bus(mut self, event_bus: Arc<crate::events::EventBus>) -> Self {
        self.event_bus = event_bus;
        self
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<crate::events::SyncEvent> {
        self.event_bus.subscribe()
    }

    /// T6-07: identifica de qual namespace é uma task de manutenção em
    /// background quando ela loga algo (`nexofsd` roda uma por conta
    /// montada) — sem isso, `nexofsd` não teria como saber a que conta um
    /// `SyncCore` recebido só como `Arc<SyncCore>` pertence.
    pub fn namespace_id(&self) -> NamespaceId {
        self.ctx.namespace_id
    }

    fn rate_scope(&self, operation_class: OperationClass) -> RateScope {
        RateScope {
            provider_id: self.ctx.provider_id.clone(),
            account_id: self.ctx.account_id,
            organization_scope: None,
            namespace_id: Some(self.ctx.namespace_id),
            operation_class,
        }
    }

    /// Garante a existência do item-raiz local (âncora da árvore, mapeado
    /// para o inode 1 do FUSE) — nunca corresponde a um item remoto real,
    /// apenas ao ponto de partida para `list_children(parent=None)`.
    pub async fn bootstrap_root(&self) -> Result<ItemId, SyncError> {
        if let Some(id) = self.root_item_id.get() {
            return Ok(*id);
        }

        let namespace_id_s = self.ctx.namespace_id.to_string();
        let existing = self
            .store
            .read(move |conn| {
                conn.query_row(
                    "SELECT item_id FROM items WHERE namespace_id = ?1 AND parent_item_id IS NULL AND remote_item_id IS NULL",
                    [namespace_id_s],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .await?;

        let root_id = match existing {
            Some(id) => parse_item_id(&id),
            None => {
                let new_id = ItemId::new();
                let item_id_s = new_id.to_string();
                let namespace_id_s = self.ctx.namespace_id.to_string();
                let now = now_unix();
                self.store
                    .write(move |tx| {
                        tx.execute(
                            "INSERT INTO items (item_id, namespace_id, remote_item_id, parent_item_id, name, normalized_name, item_type, size_bytes, children_state, remote_state, source_layer, created_at, updated_at) \
                             VALUES (?1, ?2, NULL, NULL, '', '', 'DIRECTORY', 0, 'UNKNOWN', 'PRESENT', 'REMOTE', ?3, ?3)",
                            params![item_id_s, namespace_id_s, now],
                        )
                    })
                    .await?;
                new_id
            }
        };

        let _ = self.root_item_id.set(root_id);
        Ok(root_id)
    }

    pub async fn get_item(&self, item_id: ItemId) -> Result<Option<IndexedItem>, SyncError> {
        let id_s = item_id.to_string();
        let query = format!("SELECT {ITEM_COLUMNS} FROM {FROM_ITEMS} WHERE items.item_id = ?1");
        let row = self
            .store
            .read(move |conn| conn.query_row(&query, [id_s], row_to_item).optional())
            .await?;
        Ok(row)
    }

    async fn find_item_by_remote_id(&self, remote_item_id: &str) -> Result<Option<IndexedItem>, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let remote_item_id = remote_item_id.to_string();
        let query = format!("SELECT {ITEM_COLUMNS} FROM {FROM_ITEMS} WHERE items.namespace_id = ?1 AND items.remote_item_id = ?2");
        let row = self
            .store
            .read(move |conn| {
                conn.query_row(&query, params![namespace_id_s, remote_item_id], row_to_item)
                    .optional()
            })
            .await?;
        Ok(row)
    }

    pub async fn list_children(&self, parent_item_id: ItemId) -> Result<Vec<IndexedItem>, SyncError> {
        self.ensure_children_loaded(parent_item_id).await?;

        let namespace_id_s = self.ctx.namespace_id.to_string();
        let parent_id_s = parent_item_id.to_string();
        let query = format!(
            "SELECT {ITEM_COLUMNS} FROM {FROM_ITEMS} WHERE items.namespace_id = ?1 AND items.parent_item_id = ?2 AND items.remote_state <> 'DELETED' AND {NOT_DELETED_LOCALLY} ORDER BY items.normalized_name"
        );

        let rows = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(&query)?;
                let rows = stmt.query_map(params![namespace_id_s, parent_id_s], row_to_item)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;
        Ok(rows)
    }

    pub async fn lookup_child(
        &self,
        parent_item_id: ItemId,
        name: &str,
    ) -> Result<Option<IndexedItem>, SyncError> {
        self.ensure_children_loaded(parent_item_id).await?;

        let namespace_id_s = self.ctx.namespace_id.to_string();
        let parent_id_s = parent_item_id.to_string();
        let normalized = name.to_lowercase();
        let query = format!(
            "SELECT {ITEM_COLUMNS} FROM {FROM_ITEMS} WHERE items.namespace_id = ?1 AND items.parent_item_id = ?2 AND items.normalized_name = ?3 AND items.remote_state <> 'DELETED' AND {NOT_DELETED_LOCALLY}"
        );

        let row = self
            .store
            .read(move |conn| {
                conn.query_row(&query, params![namespace_id_s, parent_id_s, normalized], row_to_item)
                    .optional()
            })
            .await?;
        Ok(row)
    }

    /// FR-IDX-003: só enumera filhos na primeira visita à pasta; visitas
    /// seguintes são atendidas pelo índice local sem nova chamada remota.
    /// FR-API-003: N chamadas concorrentes para a mesma pasta ainda não
    /// carregada resultam em uma única listagem real — as demais esperam o
    /// lock e releem o índice já populado (double-checked locking).
    async fn ensure_children_loaded(&self, parent_item_id: ItemId) -> Result<(), SyncError> {
        let parent = self.get_item(parent_item_id).await?.ok_or(SyncError::NotFound)?;
        if parent.children_loaded {
            return Ok(());
        }

        let _guard = self.loading_locks.lock(parent_item_id).await;

        let parent = self.get_item(parent_item_id).await?.ok_or(SyncError::NotFound)?;
        if parent.children_loaded {
            return Ok(());
        }

        let priority = OperationClass::InteractiveMetadata.default_priority();
        let mut page_token: Option<String> = None;
        loop {
            let account = self.account_ctx.read().await.clone();
            let namespace_remote_id = self.ctx.namespace_remote_id.clone();
            let parent_remote_item_id = parent.remote_item_id.clone().map(RemoteItemId::from);
            let request_page_token = page_token.clone();
            let provider = self.provider.clone();

            let page = self
                .execute_governed(self.rate_scope(OperationClass::InteractiveMetadata), priority, move || async move {
                    provider
                        .list_children(ListChildrenRequest {
                            account,
                            namespace_remote_id,
                            parent_remote_item_id,
                            page_token: request_page_token,
                        })
                        .await
                })
                .await?;

            for remote_item in page.items {
                self.upsert_item(parent_item_id, remote_item).await?;
            }

            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        self.mark_children_loaded(parent_item_id).await?;
        self.event_bus.publish(crate::events::SyncEvent::FolderListed {
            namespace_id: self.ctx.namespace_id,
            item_id: parent_item_id,
            name: parent.name.clone(),
        });
        Ok(())
    }

    async fn upsert_item(&self, parent_item_id: ItemId, remote_item: RemoteItem) -> Result<(), SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let parent_id_s = parent_item_id.to_string();
        let remote_id_s = remote_item.remote_item_id.0.clone();
        let name = remote_item.name.clone();
        let normalized = name.to_lowercase();
        let kind_s = item_kind_to_sql(remote_item.kind);
        let size = remote_item.size_bytes as i64;
        let version = remote_item.remote_version.clone();
        let content_version = remote_item.remote_content_version.clone();
        let modified_at = remote_item.remote_modified_at_unix;
        let now = now_unix();

        // T4-07 generalizado (bug real de produção): não é só um item
        // `LocalOnly` sob regra de exclusão que não pode ser silenciosamente
        // sobrescrito por um item remoto recém-descoberto de mesmo nome —
        // qualquer item comum criado localmente e ainda não enviado
        // (`remote_item_id IS NULL`) também ocupa a mesma vaga
        // `(pasta, nome)`. Sem esta checagem, o `INSERT`/`UPDATE` abaixo
        // batia direto na constraint `UNIQUE(namespace_id, parent_item_id,
        // normalized_name)` — e como isso propagava um erro pra fora do
        // laço de `apply_changes_from`, o cursor de mudanças nunca avançava
        // passado esse ponto: a sincronização da conta inteira ficava
        // travada para sempre, não só este item. Vira colisão estruturada
        // em vez disso — a mudança remota é reaplicada normalmente quando a
        // pasta pai for relistada, então pular aqui não perde informação.
        let colliding_item: Option<String> = self
            .store
            .read({
                let namespace_id_s = namespace_id_s.clone();
                let parent_id_s = parent_id_s.clone();
                let normalized = normalized.clone();
                let remote_id_s = remote_id_s.clone();
                move |conn| {
                    conn.query_row(
                        "SELECT item_id FROM items WHERE namespace_id = ?1 AND parent_item_id = ?2 AND normalized_name = ?3 AND remote_state <> 'DELETED' AND (remote_item_id IS NULL OR remote_item_id <> ?4)",
                        params![namespace_id_s, parent_id_s, normalized, remote_id_s],
                        |row| row.get(0),
                    )
                    .optional()
                }
            })
            .await?;
        if let Some(colliding_item_id) = colliding_item {
            self.record_conflict(
                parse_item_id(&colliding_item_id),
                nexofs_domain::states::ConflictType::LocalOnlyRemoteCollision,
                "um item remoto com o mesmo nome apareceu nesta pasta antes deste item local ter sido enviado",
            )
            .await?;
            return Ok(());
        }

        self.store
            .write(move |tx| {
                let existing_id: Option<String> = tx
                    .query_row(
                        "SELECT item_id FROM items WHERE namespace_id = ?1 AND remote_item_id = ?2",
                        params![namespace_id_s, remote_id_s],
                        |row| row.get(0),
                    )
                    .optional()?;

                match existing_id {
                    Some(id) => {
                        // Bug real de produção: uma página de `list_changes`
                        // pode chegar fora de ordem em relação a uma
                        // operação nossa recente (ex.: um `MOVE_ITEM` já
                        // confirmado avançou a versão, mas uma página de
                        // delta ainda em trânsito descreve a posição
                        // ANTERIOR ao move) — sem checar isso, este UPDATE
                        // sobrescrevia `parent_item_id`/`name` de volta para
                        // o estado velho, revertendo silenciosamente um
                        // move que o usuário via corretamente aplicado no
                        // provedor. `version_ordinal` extrai o sufixo
                        // numérico comum a todo formato de versão observado
                        // (`"N"` do FakeProvider, `"{GUID},N"` do Graph);
                        // quando não dá pra comparar (formato desconhecido,
                        // ou de qualquer lado), aplica normalmente — só
                        // recusa quando tem certeza de que é regressão.
                        let current_version: Option<String> =
                            tx.query_row("SELECT remote_version FROM items WHERE item_id = ?1", [&id], |row| row.get(0))?;
                        let is_stale = match (version_ordinal(current_version.as_deref()), version_ordinal(version.as_deref())) {
                            (Some(current_n), Some(incoming_n)) => incoming_n < current_n,
                            _ => false,
                        };
                        if !is_stale {
                            tx.execute(
                                "UPDATE items SET parent_item_id=?1, name=?2, normalized_name=?3, item_type=?4, size_bytes=?5, remote_version=?6, remote_content_version=?7, remote_modified_at=?8, remote_state='PRESENT', updated_at=?9 WHERE item_id=?10",
                                params![parent_id_s, name, normalized, kind_s, size, version, content_version, modified_at, now, id],
                            )?;
                        }
                    }
                    None => {
                        let new_id = uuid::Uuid::new_v4().to_string();
                        tx.execute(
                            "INSERT INTO items (item_id, namespace_id, remote_item_id, parent_item_id, name, normalized_name, item_type, size_bytes, remote_version, remote_content_version, remote_modified_at, children_state, remote_state, source_layer, created_at, updated_at) \
                             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'UNKNOWN','PRESENT','REMOTE',?12,?12)",
                            params![new_id, namespace_id_s, remote_id_s, parent_id_s, name, normalized, kind_s, size, version, content_version, modified_at, now],
                        )?;
                    }
                }
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn mark_children_loaded(&self, item_id: ItemId) -> Result<(), SyncError> {
        let id_s = item_id.to_string();
        self.store
            .write(move |tx| tx.execute("UPDATE items SET children_state = 'LOADED' WHERE item_id = ?1", [id_s]))
            .await?;
        Ok(())
    }

    /// FR-HYD-001/002: baixa para um arquivo temporário, valida e promove
    /// atomicamente; chamadas repetidas (ou concorrentes, via
    /// `hydrating_locks`) para o mesmo item já hidratado não tocam a rede.
    pub async fn open_and_hydrate(&self, item_id: ItemId) -> Result<PathBuf, SyncError> {
        self.open_and_hydrate_with_priority(item_id, OperationClass::InteractiveDownload).await
    }

    /// T4-10/FR-PIN-002: mesma hidratação de `open_and_hydrate`, mas numa
    /// classe de prioridade diferente — fixação recursiva usa
    /// `BackgroundIndex` (a mais baixa disponível para leitura) para não
    /// competir com um download interativo de verdade disparado por um
    /// `open()` real do usuário.
    async fn open_and_hydrate_with_priority(&self, item_id: ItemId, priority_class: OperationClass) -> Result<PathBuf, SyncError> {
        let item = self.get_item(item_id).await?.ok_or(SyncError::NotFound)?;
        if item.kind == ItemKind::Directory {
            return Err(SyncError::InvalidOperation("não é possível ler um diretório como arquivo"));
        }

        let cache_object_id = item_id.to_string();
        // T4-05: um item `LocalOnly` nunca tem `remote_item_id` — sua
        // verdade está inteiramente no overlay, nunca no cache remoto.
        if item.source_layer == "LOCAL_ONLY" {
            self.touch_access(item_id).await?;
            return Ok(self.overlay.path_for(&cache_object_id));
        }
        // Um item Dirty tem sua verdade no conteúdo local — abrir para
        // leitura precisa enxergar a escrita ainda não enviada, nunca cair
        // no caminho de hidratação remota (que, para um arquivo local nunca
        // sincronizado, nem tem `remote_item_id` para buscar). Bug real
        // encontrado validando a Fase 3: `cat` de um arquivo recém-criado
        // pelo FUSE retornava `EISDIR`, porque este método seguia direto
        // para a branch de download achando que "não hidratado" só podia
        // significar "precisa baixar".
        if self.cache.has_dirty(&cache_object_id) {
            self.touch_access(item_id).await?;
            return Ok(self.cache.dirty_path(&cache_object_id));
        }
        if self.cache.is_hydrated(&cache_object_id) {
            self.touch_access(item_id).await?;
            return Ok(self.cache.clean_path(&cache_object_id));
        }

        let _guard = self.hydrating_locks.lock(item_id).await;
        if self.cache.is_hydrated(&cache_object_id) {
            self.touch_access(item_id).await?;
            return Ok(self.cache.clean_path(&cache_object_id));
        }

        let remote_item_id = item
            .remote_item_id
            .clone()
            .ok_or(SyncError::InvalidOperation("item local ainda sem contrapartida remota"))?;

        // SPEC §19.4 item 3: antes de gastar qualquer banda, não depois.
        self.refuse_if_hydration_too_large_for_emergency(item.size_bytes)?;

        let priority = priority_class.default_priority();
        let account = self.account_ctx.read().await.clone();
        let namespace_remote_id = self.ctx.namespace_remote_id.clone();
        let provider = self.provider.clone();
        let cache = self.cache.clone();
        let cache_object_id_for_download = cache_object_id.clone();

        // A vaga de concorrência do Governor precisa cobrir o download
        // inteiro (abrir a conexão + consumir todo o corpo), não só a
        // abertura — caso contrário o limite de "N downloads interativos
        // simultâneos" (SPEC §7.8) não protege nada, e uma transferência
        // que trava sem nunca completar (rede instável, sem timeout no
        // cliente HTTP) vaza a vaga para sempre. Bug real encontrado
        // validando a Fase 2 contra o OneDrive de verdade: `in_flight`
        // ficava preso em 1 mesmo sem qualquer atividade de rede (`rchar`
        // do processo praticamente parado).
        let path = self
            .execute_governed(self.rate_scope(priority_class), priority, move || async move {
                let handle = provider
                    .open_download(DownloadRequest {
                        account,
                        namespace_remote_id,
                        remote_item_id: RemoteItemId::from(remote_item_id),
                        range: None,
                    })
                    .await?;

                cache.hydrate(&cache_object_id_for_download, handle).await.map_err(|err| {
                    nexofs_provider_api::ProviderError::new(
                        nexofs_provider_api::ProviderErrorKind::Network,
                        err.to_string(),
                    )
                })
            })
            .await?;

        let item_id_s = item_id.to_string();
        let cache_object_id_s = cache_object_id.clone();
        let size_bytes = item.size_bytes as i64;
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO local_states (item_id, hydration_state, pin_state, sync_state, cache_object_id, local_size_bytes, last_access_at, open_handle_count, updated_at) \
                     VALUES (?1, 'HYDRATED', 'ONLINE_ONLY', 'CLEAN', ?2, ?3, ?4, 0, ?4) \
                     ON CONFLICT(item_id) DO UPDATE SET hydration_state = 'HYDRATED', cache_object_id = excluded.cache_object_id, local_size_bytes = excluded.local_size_bytes, last_access_at = excluded.last_access_at, updated_at = excluded.updated_at",
                    params![item_id_s, cache_object_id_s, size_bytes, now],
                )
            })
            .await?;

        Ok(path)
    }

    /// Estado de hidratação atual (`HYDRATED`, `EVICTED`, ...) ou `None`
    /// quando o item nunca foi tocado localmente — usado por diagnóstico e
    /// por testes que precisam distinguir "hidratado" de "rehidratado por
    /// uma chamada incidental".
    pub async fn hydration_state_of(&self, item_id: ItemId) -> Result<Option<String>, SyncError> {
        let item_id_s = item_id.to_string();
        let state = self
            .store
            .read(move |conn| {
                conn.query_row(
                    "SELECT hydration_state FROM local_states WHERE item_id = ?1",
                    [item_id_s],
                    |row| row.get(0),
                )
                .optional()
            })
            .await?;
        Ok(state)
    }

    /// FR-CACHE-005/FR-API-006: uso agregado do cache deste namespace —
    /// diagnóstico/métricas locais.
    pub async fn cache_stats(&self) -> Result<CacheStats, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let (hydrated_items, hydrated_bytes): (i64, i64) = self
            .store
            .read(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(ls.local_size_bytes), 0) FROM local_states ls JOIN items i ON i.item_id = ls.item_id \
                     WHERE i.namespace_id = ?1 AND ls.hydration_state = 'HYDRATED'",
                    [namespace_id_s],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .await?;

        Ok(CacheStats {
            hydrated_items: hydrated_items.max(0) as u64,
            hydrated_bytes: hydrated_bytes.max(0) as u64,
        })
    }

    /// T5-07: mesma base de `cache_stats`, mas separada por camada
    /// (limpo/modificado localmente/mantido localmente) em vez de um único
    /// total — o que a tela de cache detalhada precisa mostrar.
    pub async fn cache_breakdown(&self) -> Result<CacheBreakdown, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let rows: Vec<(String, String, i64, i64)> = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT i.source_layer, ls.sync_state, COUNT(*), COALESCE(SUM(ls.local_size_bytes), 0) \
                     FROM local_states ls JOIN items i ON i.item_id = ls.item_id \
                     WHERE i.namespace_id = ?1 AND ls.hydration_state = 'HYDRATED' \
                     GROUP BY i.source_layer, ls.sync_state",
                )?;
                let rows = stmt.query_map([namespace_id_s], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;

        let mut breakdown = CacheBreakdown::default();
        for (source_layer, sync_state, count, bytes) in rows {
            let count = count.max(0) as u64;
            let bytes = bytes.max(0) as u64;
            if source_layer == "LOCAL_ONLY" {
                breakdown.overlay_items += count;
                breakdown.overlay_bytes += bytes;
            } else if sync_state == "CLEAN" {
                breakdown.clean_items += count;
                breakdown.clean_bytes += bytes;
            } else {
                breakdown.dirty_items += count;
                breakdown.dirty_bytes += bytes;
            }
        }
        Ok(breakdown)
    }

    async fn touch_access(&self, item_id: ItemId) -> Result<(), SyncError> {
        let item_id_s = item_id.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| tx.execute("UPDATE local_states SET last_access_at = ?1 WHERE item_id = ?2", params![now, item_id_s]))
            .await?;
        Ok(())
    }

    /// Chamado por `nexofs-fuse` em `open`/`release` — protege itens
    /// abertos de serem evictados por baixo do leitor (SPEC §12.5 "item com
    /// `open_handle_count > 0` NÃO PODE ser evictado").
    pub async fn mark_handle_opened(&self, item_id: ItemId) -> Result<(), SyncError> {
        let item_id_s = item_id.to_string();
        self.store
            .write(move |tx| tx.execute("UPDATE local_states SET open_handle_count = open_handle_count + 1 WHERE item_id = ?1", [item_id_s]))
            .await?;
        Ok(())
    }

    pub async fn mark_handle_closed(&self, item_id: ItemId) -> Result<(), SyncError> {
        let item_id_s = item_id.to_string();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE local_states SET open_handle_count = MAX(open_handle_count - 1, 0) WHERE item_id = ?1",
                    [item_id_s],
                )
            })
            .await?;
        Ok(())
    }

    /// FR-CACHE-001/002: quando o total hidratado ultrapassa `max_bytes`,
    /// evicta pelos mais antigos (`last_access_at` ASC) entre os elegíveis.
    ///
    /// T4-11 (SPEC §12.5): elegibilidade agora exclui explicitamente
    /// `pinned`/`dirty`/`conflito`/`LocalOnly`, além de "sem handle aberto"
    /// (já existia desde a Fase 2) — antes desta correção, a query só
    /// filtrava por `hydration_state = 'HYDRATED'`, que um item `Dirty` ou
    /// `Conflict` também tem (setado por `begin_write`/`record_conflict`);
    /// sob pressão real de cache, um arquivo com edição local não enviada
    /// podia ser "evictado" (removido de `clean/`, o que não continha o
    /// conteúdo real de qualquer forma — mas `local_size_bytes` era zerado,
    /// corrompendo o tamanho reportado por `getattr` para um arquivo que
    /// continuava presente e íntegro em `dirty/`). Também corrigido para
    /// somar/evictar só dentro do próprio namespace — `local_states` é
    /// compartilhado por todas as contas no mesmo `nexofs.sqlite3`.
    pub async fn enforce_cache_quota(&self, max_bytes: u64) -> Result<(), SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let total: i64 = self
            .store
            .read({
                let namespace_id_s = namespace_id_s.clone();
                move |conn| {
                    conn.query_row(
                        "SELECT COALESCE(SUM(ls.local_size_bytes), 0) FROM local_states ls JOIN items i ON i.item_id = ls.item_id \
                         WHERE i.namespace_id = ?1 AND ls.hydration_state = 'HYDRATED'",
                        [namespace_id_s],
                        |row| row.get(0),
                    )
                }
            })
            .await?;

        if total < 0 || total as u64 <= max_bytes {
            return Ok(());
        }

        let mut to_free = total as u64 - max_bytes;
        let candidates: Vec<(String, String, i64)> = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT ls.item_id, COALESCE(ls.cache_object_id, ''), COALESCE(ls.local_size_bytes, 0) \
                     FROM local_states ls JOIN items i ON i.item_id = ls.item_id \
                     WHERE i.namespace_id = ?1 AND ls.hydration_state = 'HYDRATED' AND ls.open_handle_count = 0 \
                     AND ls.sync_state = 'CLEAN' AND ls.pin_state <> 'PINNED' AND i.source_layer <> 'LOCAL_ONLY' \
                     ORDER BY ls.last_access_at ASC, ls.rowid ASC",
                )?;
                let rows = stmt.query_map([namespace_id_s], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;

        for (item_id_s, cache_object_id, size) in candidates {
            if to_free == 0 {
                break;
            }
            self.cache.remove(&cache_object_id).map_err(CacheError::from)?;
            self.store
                .write(move |tx| {
                    tx.execute(
                        "UPDATE local_states SET hydration_state = 'EVICTED', cache_object_id = NULL, local_size_bytes = NULL WHERE item_id = ?1",
                        [item_id_s],
                    )
                })
                .await?;
            to_free = to_free.saturating_sub(size.max(0) as u64);
        }

        Ok(())
    }

    /// FR-ACT-001/002/003: registra atividade recente para a pasta e, se
    /// `is_interactive` (decidido por quem chama — em `nexofs-fuse`, pela
    /// classificação do processo via `/proc`) e já passou o intervalo
    /// mínimo desde o último disparo, agenda uma verificação incremental em
    /// segundo plano (fire-and-forget: quem navega não espera a rede).
    /// Chamadas de thumbnailers/indexadores conhecidos passam
    /// `is_interactive = false` e nunca disparam refresh (FR-ACT-003).
    pub fn mark_directory_active(self: &Arc<Self>, item_id: ItemId, is_interactive: bool) {
        if !is_interactive {
            return;
        }

        {
            let mut sessions = self
                .active_directories
                .lock()
                .expect("lock não é mantido durante ponto de await");
            sessions.insert(item_id, Instant::now());
        }

        let should_trigger = {
            let mut last = self.last_activity_refresh_at.lock().expect("lock síncrono");
            let now = Instant::now();
            let due = last.is_none_or(|t| now.duration_since(t) >= ACTIVITY_MIN_INTERVAL);
            if due {
                *last = Some(now);
            }
            due
        };

        if should_trigger {
            let core = self.clone();
            self.runtime_handle.spawn(async move {
                if let Err(err) = core.refresh_changes().await {
                    tracing::warn!(%err, "falha ao verificar mudanças disparada por atividade de navegação");
                }
            });
        }
    }

    pub fn active_directory_count(&self) -> usize {
        self.active_directories.lock().expect("lock síncrono").len()
    }

    /// FR-REF-004/FR-IDX-004: sincronização incremental via cursor. Na
    /// primeira execução (`cursor_state = UNINITIALIZED`), obtém um cursor
    /// "a partir de agora" sem enumerar histórico algum — a árvore já foi
    /// (ou será) populada pela indexação lazy via `list_children`. Chamadas
    /// seguintes aplicam apenas o que mudou desde o último cursor válido.
    pub async fn refresh_changes(&self) -> Result<(), SyncError> {
        let result = self.refresh_changes_inner().await;
        if result.is_ok() {
            self.event_bus.publish(crate::events::SyncEvent::RefreshCompleted { namespace_id: self.ctx.namespace_id });
        }
        result
    }

    async fn refresh_changes_inner(&self) -> Result<(), SyncError> {
        let (cursor_state, cursor_value) = self.read_cursor_state().await?;

        match cursor_state.as_str() {
            "UNINITIALIZED" => {
                let cursor = self.request_change_cursor(true).await?;
                self.persist_cursor(&cursor, "VALID").await?;
                Ok(())
            }
            "REBUILDING" | "ERROR" => self.reconcile_cursor().await,
            _ => {
                let Some(cursor_value) = cursor_value else {
                    return self.reconcile_cursor().await;
                };
                self.apply_changes_from(ChangeCursor(cursor_value)).await
            }
        }
    }

    async fn read_cursor_state(&self) -> Result<(String, Option<String>), SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let row: (String, Option<String>) = self
            .store
            .read(move |conn| {
                conn.query_row(
                    "SELECT cursor_state, change_cursor FROM namespaces WHERE namespace_id = ?1",
                    [namespace_id_s],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .await?;
        Ok(row)
    }

    async fn request_change_cursor(&self, latest_only: bool) -> Result<ChangeCursor, SyncError> {
        let account = self.account_ctx.read().await.clone();
        let namespace_remote_id = self.ctx.namespace_remote_id.clone();
        let provider = self.provider.clone();
        let priority = OperationClass::ChangeTracking.default_priority();

        let cursor = self
            .execute_governed(self.rate_scope(OperationClass::ChangeTracking), priority, move || async move {
                provider
                    .create_change_cursor(CreateCursorRequest {
                        account,
                        namespace_remote_id,
                        latest_only,
                    })
                    .await
            })
            .await?;
        Ok(cursor)
    }

    async fn persist_cursor(&self, cursor: &ChangeCursor, state: &str) -> Result<(), SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let cursor_value = cursor.0.clone();
        let state = state.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE namespaces SET change_cursor = ?1, cursor_state = ?2, last_change_check_at = ?3 WHERE namespace_id = ?4",
                    params![cursor_value, state, now, namespace_id_s],
                )
            })
            .await?;
        Ok(())
    }

    /// SPEC §14.3: cursor expirado/corrompido não apaga a árvore existente
    /// — obtém um novo cursor "a partir de agora" e segue dali, mantendo o
    /// usuário com acesso à visão atual durante a reconstrução.
    async fn reconcile_cursor(&self) -> Result<(), SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        self.store
            .write(move |tx| tx.execute("UPDATE namespaces SET cursor_state = 'REBUILDING' WHERE namespace_id = ?1", [namespace_id_s]))
            .await?;

        let cursor = self.request_change_cursor(true).await?;
        self.persist_cursor(&cursor, "VALID").await?;
        tracing::warn!(namespace_id = %self.ctx.namespace_id, "cursor reconstruído a partir de agora — mudanças anteriores à reconstrução não serão reaplicadas nesta versão");
        Ok(())
    }

    /// Aplica páginas de `list_changes` em sequência até alcançar o cursor
    /// corrente (`has_more = false`), persistindo o próximo cursor somente
    /// após cada página inteira ser aplicada (SPEC §14.2).
    async fn apply_changes_from(&self, mut cursor: ChangeCursor) -> Result<(), SyncError> {
        let priority = OperationClass::ChangeTracking.default_priority();
        loop {
            let account = self.account_ctx.read().await.clone();
            let namespace_remote_id = self.ctx.namespace_remote_id.clone();
            let provider = self.provider.clone();
            let request_cursor = cursor.clone();

            let page_result = self
                .execute_governed(self.rate_scope(OperationClass::ChangeTracking), priority, move || async move {
                    provider
                        .list_changes(ListChangesRequest {
                            account,
                            namespace_remote_id,
                            cursor: request_cursor,
                        })
                        .await
                })
                .await;

            let page = match page_result {
                Ok(page) => page,
                Err(err) if err.kind == nexofs_provider_api::ProviderErrorKind::CorruptResponse => {
                    return self.reconcile_cursor().await;
                }
                Err(err) => return Err(err.into()),
            };

            for change in page.changes {
                self.apply_remote_change(change).await?;
            }

            self.persist_cursor(&page.next_cursor, "VALID").await?;
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }
        Ok(())
    }

    async fn apply_remote_change(&self, change: RemoteChange) -> Result<(), SyncError> {
        match change {
            RemoteChange::Upserted(remote_item) => {
                let parent_item_id = match &remote_item.parent_remote_item_id {
                    None => self.bootstrap_root().await?,
                    Some(parent_remote_id) => match self.find_item_by_remote_id(parent_remote_id.as_ref()).await? {
                        Some(parent) => parent.item_id,
                        None => {
                            // Pai ainda não indexado localmente (pasta nunca
                            // visitada) — aplicar esta mudança exigiria
                            // fabricar metadados que não temos. Resolução
                            // completa da cadeia de pais fica para
                            // hardening futuro; por ora, a mudança será
                            // capturada quando a pasta pai for listada.
                            tracing::debug!(
                                remote_item_id = remote_item.remote_item_id.as_ref(),
                                "mudança ignorada — pai ainda não indexado localmente"
                            );
                            return Ok(());
                        }
                    },
                };
                self.upsert_item(parent_item_id, remote_item).await
            }
            RemoteChange::Deleted { remote_item_id } => self.tombstone_item(remote_item_id).await,
        }
    }

    /// Exclusão remota vira tombstone (`remote_state = 'DELETED'`), nunca
    /// uma remoção física — preserva histórico e evita recriar o item se um
    /// upload local ainda pendente tentar referenciá-lo (Fase 3).
    ///
    /// T3-08/SPEC §18.2 (segunda cláusula: "quando uma operação não pode ser
    /// mapeada sem descartar dados"): um item ainda `Dirty` localmente (edit
    /// nunca enviado) NÃO é tombstoneado — `remote_state <> 'DELETED'` é
    /// exatamente o filtro que `list_children`/`lookup_child` usam para
    /// decidir o que aparece no mount, então tombstonear aqui faria a edição
    /// do usuário desaparecer silenciosamente da árvore antes mesmo de
    /// qualquer upload ter a chance de detectar o conflito. Em vez disso o
    /// item continua visível com seu conteúdo dirty intacto e ganha um
    /// conflito `RemoteDeletedLocalModified` estruturado.
    async fn tombstone_item(&self, remote_item_id: RemoteItemId) -> Result<(), SyncError> {
        let Some(item) = self.find_item_by_remote_id(remote_item_id.as_ref()).await? else {
            return Ok(());
        };

        if matches!(item.sync_state.as_deref(), Some("DIRTY") | Some("UPLOAD_QUEUED") | Some("UPLOADING")) {
            self.record_conflict(
                item.item_id,
                nexofs_domain::states::ConflictType::RemoteDeletedLocalModified,
                "item apagado remotamente enquanto havia uma edição local não enviada",
            )
            .await?;
            return Ok(());
        }

        let namespace_id_s = self.ctx.namespace_id.to_string();
        let remote_id_s = remote_item_id.0;
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE items SET remote_state = 'DELETED', updated_at = ?1 WHERE namespace_id = ?2 AND remote_item_id = ?3",
                    params![now, namespace_id_s, remote_id_s],
                )
            })
            .await?;
        Ok(())
    }
}
