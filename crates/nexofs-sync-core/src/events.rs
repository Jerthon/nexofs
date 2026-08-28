//! Barramento de eventos em tempo real (T5-02, SPEC §20.4). Um único
//! `broadcast` é compartilhado por todos os `SyncCore`/namespaces montados
//! no processo (via `SyncCore::with_event_bus`), para que `nexofs-local-api`
//! exponha um único `GET /v1/events` em vez de um stream por namespace.
//!
//! Cobre os pontos hoje observáveis pelo núcleo sem plumbing novo:
//! progresso de operação, conflito criado/resolvido, refresh concluído e
//! mudança de nível de pressão de disco. "Transfer progress" (byte a byte)
//! e "authentication required" ficam pendentes — dependem de capacidades
//! que `CloudProvider`/o fluxo de login ainda não expõem (ver TASKS.md).

use nexofs_domain::{ConflictId, ItemId, NamespaceId, OperationId};
use serde::Serialize;
use tokio::sync::broadcast;

/// Generoso o bastante para um período de reconexão de um assinante lento
/// sem perder eventos com frequência — um assinante que fique para trás
/// disso recebe `Lagged` e resincroniza via uma chamada REST normal (o
/// stream é um atalho de UX, nunca a única fonte de verdade).
const CHANNEL_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SyncEvent {
    NamespaceMounted { namespace_id: NamespaceId },
    NamespaceUnmounted { namespace_id: NamespaceId },
    /// `item_name`/`item_path` (T5-desktop, "mostrar o que está sendo
    /// feito"): resolvidos na hora da publicação, a partir do índice — por
    /// isso podem vir `None` quando o item já não existe mais no momento do
    /// evento (ex.: uma exclusão bem-sucedida já removeu a própria linha de
    /// `items`, ver `dispatch_delete`/`hard_delete_item`).
    OperationProgress {
        namespace_id: NamespaceId,
        operation_id: OperationId,
        operation_type: Option<String>,
        state: String,
        item_name: Option<String>,
        item_path: Option<String>,
    },
    ConflictCreated { namespace_id: NamespaceId, conflict_id: ConflictId, item_id: ItemId },
    ConflictResolved { namespace_id: NamespaceId, conflict_id: ConflictId },
    RefreshCompleted { namespace_id: NamespaceId },
    CachePressureChanged { namespace_id: NamespaceId, level: String },
    /// T5-desktop ("log de sincronização"): emitido só quando uma pasta é
    /// enumerada de verdade pelo provedor pela primeira vez (FR-IDX-003) —
    /// nunca em visitas seguintes servidas só pelo índice local — para a UI
    /// poder mostrar ao usuário o carregamento lazy acontecendo pasta por
    /// pasta em vez de tudo de uma vez.
    FolderListed { namespace_id: NamespaceId, item_id: ItemId, name: String },
}

pub struct EventBus {
    sender: broadcast::Sender<SyncEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, _receiver) = broadcast::channel(CHANNEL_CAPACITY);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.sender.subscribe()
    }

    /// Nenhum assinante no momento não é erro — eventos existem para quem
    /// estiver ouvindo (UI/CLI/testes), não há obrigação de haver um.
    pub fn publish(&self, event: SyncEvent) {
        let _ = self.sender.send(event);
    }
}
