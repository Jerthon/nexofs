use nexofs_api_governor::Deduplicator;
use nexofs_domain::{AccountId, NamespaceId};
use nexofs_sync_core::{EventBus, SyncCore};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};

/// T5-01/SPEC §20.3 `GET /v1/accounts` — nome e provedor de uma conta
/// montada; nunca inclui token/refresh token (NFR-SEC-002/006).
#[derive(Clone, Serialize)]
pub struct AccountSummary {
    pub account_id: AccountId,
    pub provider_id: String,
    pub display_name: String,
}

/// T5-01/SPEC §20.3 `GET /v1/namespaces`.
#[derive(Clone, Serialize)]
pub struct NamespaceSummary {
    pub namespace_id: NamespaceId,
    pub account_id: AccountId,
    pub display_name: String,
    pub mount_path: String,
    pub mount_state: String,
}

/// As três listas mudam sempre juntas (uma conta nova sempre chega com seu
/// namespace e seu resumo) — um único `RwLock` em vez de três evita ficar
/// visível um estado parcialmente atualizado entre eles.
#[derive(Default)]
pub struct MountedState {
    pub namespaces: HashMap<NamespaceId, Arc<SyncCore>>,
    pub accounts: Vec<AccountSummary>,
    pub namespace_summaries: Vec<NamespaceSummary>,
}

/// T5-desktop ("adicionar conta") — a API local não sabe autenticar no
/// OneDrive nem montar FUSE (isso é responsabilidade de `nexofsd::main`,
/// ADR-005: o daemon é dono do ciclo de vida das montagens, a API local só
/// fala HTTP). Este canal é como o handler de `POST /v1/accounts/auth/start`
/// pede pro dono de verdade fazer o trabalho e espera o resultado de volta.
pub struct AddAccountRequest {
    /// T7-02: qual adaptador usar (`"onedrive"`/`"googledrive"`) — escolhido
    /// pelo usuário na tela de "adicionar conta", nunca inferido.
    pub provider_id: String,
    /// Onde montar (padrão `$HOME/NexoFS/<nome>` quando `None`) e como
    /// nomear o novo namespace — ambos escolhidos pelo usuário na tela de
    /// "adicionar conta", nunca inferidos silenciosamente quando informados.
    pub mount_path: Option<std::path::PathBuf>,
    pub display_name: Option<String>,
    pub respond_to: oneshot::Sender<Result<NamespaceSummary, String>>,
}

/// T5-desktop ("desmontar"/"remontar"/"excluir conta"): mesmo padrão de
/// `AddAccountRequest` só que para as três ações que operam sobre uma conta
/// já existente — um único canal para as três porque nenhuma delas fala com
/// o navegador (ao contrário de "adicionar", que pode ficar minutos
/// esperando o login), então não há motivo para serializar cada uma na sua
/// própria fila.
pub enum AccountControlRequest {
    Unmount { account_id: AccountId, respond_to: oneshot::Sender<Result<(), String>> },
    Remount { account_id: AccountId, respond_to: oneshot::Sender<Result<NamespaceSummary, String>> },
    Delete { account_id: AccountId, respond_to: oneshot::Sender<Result<(), String>> },
}

/// Estado do servidor. `mounted` é a única parte que muda em runtime (a
/// chegada de uma conta nova via `POST /v1/accounts/auth/start`); o resto é
/// fixo desde a inicialização.
#[derive(Clone)]
pub struct AppState {
    pub mounted: Arc<RwLock<MountedState>>,
    /// `None` em instâncias que não sabem adicionar conta (todos os testes
    /// de integração, que não têm um `nexofsd::main` de verdade por trás) —
    /// o handler correspondente responde com um erro claro nesse caso, em
    /// vez de exigir que todo teste monte um receptor de canal que nunca
    /// vai usar.
    pub add_account_tx: Option<mpsc::Sender<AddAccountRequest>>,
    /// `None` no mesmo caso que `add_account_tx` — testes de integração sem
    /// um `nexofsd::main` de verdade por trás.
    pub account_control_tx: Option<mpsc::Sender<AccountControlRequest>>,
    /// FR-REF-003: cliques repetidos no mesmo namespace compartilham a
    /// mesma execução de `refresh_changes` em vez de disparar N chamadas.
    pub refresh_dedup: Arc<Deduplicator<NamespaceId, bool>>,
    /// T3-04, quarto gatilho de estabilização ("comando manual") — instância
    /// própria, separada de `refresh_dedup`: são ações distintas (uma puxa
    /// mudanças remotas, a outra empurra escritas locais pendentes) e não
    /// podem compartilhar a mesma deduplicação por chave `namespace_id`.
    pub sync_now_dedup: Arc<Deduplicator<NamespaceId, bool>>,
    /// Compartilhado entre todas as contas — SPEC §7.2, mesmo `Governor`
    /// usado pelos `SyncCore`s passados em `namespaces`.
    pub governor: Arc<nexofs_api_governor::ProviderApiGovernor>,
    /// T5-02: o mesmo barramento passado a cada `SyncCore` via
    /// `with_event_bus` — `GET /v1/events` assina este, não um por conta.
    pub event_bus: Arc<EventBus>,
    /// FR-CACHE-004 — orçamento aplicado por `POST /v1/cache/cleanup`; o
    /// mesmo valor usado pelo tick periódico de manutenção do daemon.
    pub cache_max_bytes: u64,
    /// T5-10/SPEC §23.3 — onde `POST /v1/diagnostics/package` grava uma
    /// cópia de cada pacote gerado (`$XDG_DATA_HOME/nexofs/diagnostics`).
    pub diagnostics_dir: std::path::PathBuf,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        namespaces: HashMap<NamespaceId, Arc<SyncCore>>,
        accounts: Vec<AccountSummary>,
        namespace_summaries: Vec<NamespaceSummary>,
        governor: Arc<nexofs_api_governor::ProviderApiGovernor>,
        event_bus: Arc<EventBus>,
        cache_max_bytes: u64,
        diagnostics_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            mounted: Arc::new(RwLock::new(MountedState { namespaces, accounts, namespace_summaries })),
            add_account_tx: None,
            account_control_tx: None,
            refresh_dedup: Arc::new(Deduplicator::new()),
            sync_now_dedup: Arc::new(Deduplicator::new()),
            governor,
            event_bus,
            cache_max_bytes,
            diagnostics_dir,
        }
    }

    /// Chamado só por `nexofsd::main` — nenhum teste de integração precisa
    /// disto (ver o campo `add_account_tx`).
    pub fn with_add_account_channel(mut self, tx: mpsc::Sender<AddAccountRequest>) -> Self {
        self.add_account_tx = Some(tx);
        self
    }

    pub fn with_account_control_channel(mut self, tx: mpsc::Sender<AccountControlRequest>) -> Self {
        self.account_control_tx = Some(tx);
        self
    }

    pub async fn sync_core_for(&self, namespace_id: NamespaceId) -> Option<Arc<SyncCore>> {
        self.mounted.read().await.namespaces.get(&namespace_id).cloned()
    }

    /// Cópia rasa do mapa (clona só os `Arc<SyncCore>`, não os `SyncCore`
    /// em si) — usada por handlers que agregam algo de todos os namespaces
    /// montados; evita segurar o `RwLock` durante N chamadas assíncronas ao
    /// núcleo, que não precisam dele.
    pub async fn namespaces_snapshot(&self) -> HashMap<NamespaceId, Arc<SyncCore>> {
        self.mounted.read().await.namespaces.clone()
    }

    /// Chamado só por `nexofsd::main` depois de montar uma conta nova em
    /// runtime — o `namespace_id` some de `GET /v1/namespaces`/`/v1/status`
    /// nunca antes de o FUSE já estar de pé de verdade (quem chama monta
    /// primeiro, insere depois).
    /// "Insere" no sentido amplo: também é como uma conta remontada (que já
    /// tinha uma entrada em `namespace_summaries` marcada `UNMOUNTED`) volta
    /// a aparecer como `MOUNTED` — por isso substitui a entrada existente em
    /// vez de sempre empilhar uma nova.
    pub async fn insert_mounted(&self, namespace_id: NamespaceId, sync_core: Arc<SyncCore>, account: AccountSummary, namespace_summary: NamespaceSummary) {
        let mut mounted = self.mounted.write().await;
        mounted.namespaces.insert(namespace_id, sync_core);
        match mounted.accounts.iter_mut().find(|a| a.account_id == account.account_id) {
            Some(existing) => *existing = account,
            None => mounted.accounts.push(account),
        }
        match mounted.namespace_summaries.iter_mut().find(|n| n.namespace_id == namespace_id) {
            Some(existing) => *existing = namespace_summary,
            None => mounted.namespace_summaries.push(namespace_summary),
        }
    }

    /// T5-desktop ("desmontar"): tira o `SyncCore` do mapa de montados (todo
    /// endpoint por namespace passa a responder 404 para ele, igual a um
    /// namespace que nunca existiu) mas mantém a linha em
    /// `namespace_summaries` — só com `mount_state` trocado — para a tela
    /// continuar mostrando a conta com um botão de "remontar".
    pub async fn mark_unmounted(&self, namespace_id: NamespaceId) {
        let mut mounted = self.mounted.write().await;
        mounted.namespaces.remove(&namespace_id);
        if let Some(summary) = mounted.namespace_summaries.iter_mut().find(|n| n.namespace_id == namespace_id) {
            summary.mount_state = "UNMOUNTED".to_string();
        }
    }

    /// T5-desktop ("excluir conta"): some de vez das três listas — ao
    /// contrário de `mark_unmounted`, não sobra nenhum rastro em
    /// `GET /v1/accounts`/`GET /v1/namespaces`.
    pub async fn remove_account(&self, account_id: AccountId) {
        let mut mounted = self.mounted.write().await;
        let namespace_ids: Vec<NamespaceId> =
            mounted.namespace_summaries.iter().filter(|n| n.account_id == account_id).map(|n| n.namespace_id).collect();
        for namespace_id in namespace_ids {
            mounted.namespaces.remove(&namespace_id);
        }
        mounted.namespace_summaries.retain(|n| n.account_id != account_id);
        mounted.accounts.retain(|a| a.account_id != account_id);
    }
}
