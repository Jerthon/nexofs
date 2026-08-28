//! Controle de tempestade de arquivos (T4-09, SPEC §7.9/API-020, PRD §8.5).
//! Rastreia a taxa de criação de itens por pasta-mãe; ao ultrapassar o
//! limiar, pausa a geração de novas operações remotas para essa pasta (o
//! conteúdo continua sendo escrito localmente sem perda — só para de
//! competir por chamadas ao provedor) e classifica o risco. A retomada é
//! sempre explícita (`resume_from_storm_pause`), nunca automática.

use super::SyncCore;
use crate::error::SyncError;
use nexofs_domain::ItemId;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// API-020: "mais de 1.000 novos itens em 30 segundos numa mesma subárvore".
const STORM_WINDOW: Duration = Duration::from_secs(30);
const STORM_THRESHOLD: usize = 1000;
/// PRD §8.5: segundo sinal independente — uma pasta com fila de upload
/// grande demais também pede confirmação, mesmo sem uma rajada recente.
const PENDING_THRESHOLD: u64 = 10_000;

/// Nomes de pasta reconhecidos como dependência/cache — mesmos diretórios
/// dos perfis de tecnologia de T4-04 (`SPEC §17.4`), reaproveitados aqui
/// como heurística de classificação de risco ("nomes típicos de
/// dependências/cache") em vez de manter uma segunda lista.
fn looks_like_a_dependency_folder(name: &str) -> bool {
    nexofs_ignore::KNOWN_PROFILES
        .iter()
        .flat_map(|profile| profile.patterns)
        .any(|pattern| pattern.trim_end_matches('/') == name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StormRiskFactor {
    /// A própria pasta já teria sido `LocalOnly` de qualquer forma.
    AlreadyExcluded,
    /// Nome reconhecido de dependência/cache (`node_modules`, `vendor`, ...).
    DependencyLikeName,
    /// Nenhum dos dois acima — merece atenção humana antes de prosseguir.
    Unclassified,
}

#[derive(Debug, Clone, Copy)]
pub struct StormEvent {
    pub folder_item_id: ItemId,
    pub items_in_window: usize,
    pub risk: StormRiskFactor,
}

#[derive(Default)]
pub(crate) struct StormTracker {
    creations_by_parent: HashMap<ItemId, VecDeque<Instant>>,
    paused: HashSet<ItemId>,
}

impl SyncCore {
    /// Chamado por `create_local_item` a cada item novo — registra o
    /// evento e devolve `Some` só quando isto CAUSA a pasta a entrar (ou já
    /// estar) em pausa, para quem chama decidir se ainda enfileira a
    /// operação desta chamada específica.
    pub(crate) async fn record_creation_and_check_storm(&self, parent_item_id: ItemId, child_name: &str, is_local_only: bool) -> Option<StormEvent> {
        let now = Instant::now();
        let mut tracker = self.storm_tracker.lock().expect("lock síncrono, nunca mantido durante um await");

        let window = tracker.creations_by_parent.entry(parent_item_id).or_default();
        window.push_back(now);
        while let Some(oldest) = window.front() {
            if now.duration_since(*oldest) > STORM_WINDOW {
                window.pop_front();
            } else {
                break;
            }
        }
        let items_in_window = window.len();

        let already_paused = tracker.paused.contains(&parent_item_id);
        if !already_paused && items_in_window <= STORM_THRESHOLD {
            return None;
        }

        if !already_paused {
            tracker.paused.insert(parent_item_id);
            tracing::warn!(
                %parent_item_id,
                items_in_window,
                "tempestade de arquivos detectada — pausando novas operações remotas nesta pasta (SPEC §7.9)"
            );
        }

        let risk = if is_local_only {
            StormRiskFactor::AlreadyExcluded
        } else if looks_like_a_dependency_folder(child_name) {
            StormRiskFactor::DependencyLikeName
        } else {
            StormRiskFactor::Unclassified
        };
        Some(StormEvent { folder_item_id: parent_item_id, items_in_window, risk })
    }

    /// `true` quando `parent_item_id` está pausada por tempestade — quem
    /// cria/estabiliza itens dentro dela deve pular o enqueue da operação
    /// remota (o conteúdo já está seguro localmente; só a fila de rede
    /// espera) até uma retomada explícita.
    pub(crate) fn is_storm_paused(&self, parent_item_id: ItemId) -> bool {
        self.storm_tracker.lock().expect("lock síncrono").paused.contains(&parent_item_id)
    }

    /// Pastas hoje pausadas por tempestade — diagnóstico/API local.
    pub fn storm_paused_folders(&self) -> Vec<ItemId> {
        self.storm_tracker.lock().expect("lock síncrono").paused.iter().copied().collect()
    }

    /// PRD §8.5, segundo sinal: uma pasta cuja fila de upload já passou de
    /// `PENDING_THRESHOLD` itens pede a mesma pausa+confirmação, mesmo sem
    /// uma rajada recente de criação (ex.: muitas operações represadas por
    /// ficarem `WaitingRetry`/`WaitingNetwork` por um tempo).
    pub async fn check_pending_queue_storm(&self, folder_item_id: ItemId) -> Result<bool, SyncError> {
        let pending = self.pending_operations().await?;
        let mut descendants = std::collections::HashSet::new();
        let mut stack = vec![folder_item_id];
        while let Some(current) = stack.pop() {
            if !descendants.insert(current) {
                continue;
            }
            if let Ok(children) = self.list_children(current).await {
                stack.extend(children.iter().map(|c| c.item_id));
            }
        }
        let affected = pending.iter().filter(|op| op.item_id.is_some_and(|id| descendants.contains(&id))).count() as u64;
        if affected > PENDING_THRESHOLD {
            self.storm_tracker.lock().expect("lock síncrono").paused.insert(folder_item_id);
            tracing::warn!(%folder_item_id, affected, "fila de operações pendentes acima do limiar — pausando até confirmação (PRD §8.5)");
            return Ok(true);
        }
        Ok(false)
    }

    /// Retomada explícita (nunca automática) após o usuário revisar a
    /// classificação de risco — limpa a pausa e estabiliza todo item dirty
    /// pendente (reaproveita o comando manual de T3-04; um efeito colateral
    /// inofensivo é também estabilizar itens dirty de fora desta pasta que
    /// já estivessem esperando por outro motivo).
    pub async fn resume_from_storm_pause(&self, folder_item_id: ItemId) -> Result<u64, SyncError> {
        self.storm_tracker.lock().expect("lock síncrono").paused.remove(&folder_item_id);
        self.stabilize_all_dirty_items().await
    }
}
