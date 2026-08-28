use nexofs_domain::{ItemId, OperationId};
use nexofs_domain::states::{OperationState, OperationType};
use nexofs_provider_api::ItemKind;

#[derive(Debug, Clone, Copy, Default)]
pub struct CacheStats {
    pub hydrated_items: u64,
    pub hydrated_bytes: u64,
}

/// T5-07 — detalhamento do espaço em disco por camada. `partial_*` fica
/// sempre em zero nesta arquitetura de propósito: hidratação promove o
/// download para `HYDRATED` atomicamente (arquivo temporário + rename,
/// T1-10), então um download em andamento nunca fica visível no índice como
/// um estado `PARTIAL` persistido — não é um campo esquecido, é a garantia
/// "conteúdo parcial nunca é apresentado como completo" se manifestando
/// como "não existe estado parcial para contar".
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheBreakdown {
    pub clean_items: u64,
    pub clean_bytes: u64,
    pub dirty_items: u64,
    pub dirty_bytes: u64,
    pub partial_items: u64,
    pub partial_bytes: u64,
    pub overlay_items: u64,
    pub overlay_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct QueuedOperation {
    pub operation_id: OperationId,
    pub item_id: Option<ItemId>,
    pub operation_type: OperationType,
    pub state: OperationState,
    pub priority: u8,
    pub attempt_count: u32,
    pub idempotency_key: String,
    /// Versão remota que a intenção local tinha como base no momento em que
    /// foi enfileirada (congelada por quem chama `enqueue_operation` — ver
    /// `SyncCore::begin_write`). O dispatcher DEVE usar este valor, nunca
    /// reler `items.remote_version` no momento do despacho: entre o enqueue
    /// e o despacho um `refresh_changes` pode ter avançado
    /// `items.remote_version` por conta de outra mudança remota, e reler o
    /// valor já atualizado mascararia silenciosamente o próprio conflito que
    /// o controle otimista de versão existe para pegar (T3-07/T3-08).
    pub base_remote_version: Option<String>,
}

pub(crate) fn operation_type_to_sql(t: OperationType) -> &'static str {
    match t {
        OperationType::UploadFile => "UPLOAD_FILE",
        OperationType::CreateDirectory => "CREATE_DIRECTORY",
        OperationType::MoveItem => "MOVE_ITEM",
        OperationType::RenameItem => "RENAME_ITEM",
        OperationType::DeleteItem => "DELETE_ITEM",
        OperationType::RestoreItem => "RESTORE_ITEM",
        OperationType::HydrateItem => "HYDRATE_ITEM",
        OperationType::PinTree => "PIN_TREE",
        OperationType::RefreshChanges => "REFRESH_CHANGES",
        OperationType::ReconcileNamespace => "RECONCILE_NAMESPACE",
    }
}

pub(crate) fn operation_type_from_sql(value: &str) -> OperationType {
    match value {
        "UPLOAD_FILE" => OperationType::UploadFile,
        "CREATE_DIRECTORY" => OperationType::CreateDirectory,
        "MOVE_ITEM" => OperationType::MoveItem,
        "RENAME_ITEM" => OperationType::RenameItem,
        "DELETE_ITEM" => OperationType::DeleteItem,
        "RESTORE_ITEM" => OperationType::RestoreItem,
        "HYDRATE_ITEM" => OperationType::HydrateItem,
        "PIN_TREE" => OperationType::PinTree,
        "REFRESH_CHANGES" => OperationType::RefreshChanges,
        _ => OperationType::ReconcileNamespace,
    }
}

pub(crate) fn operation_state_to_sql(s: OperationState) -> &'static str {
    match s {
        OperationState::Pending => "PENDING",
        OperationState::Running => "RUNNING",
        OperationState::WaitingRetry => "WAITING_RETRY",
        OperationState::WaitingNetwork => "WAITING_NETWORK",
        OperationState::WaitingAuthentication => "WAITING_AUTHENTICATION",
        OperationState::BlockedByConflict => "BLOCKED_BY_CONFLICT",
        OperationState::Completed => "COMPLETED",
        OperationState::Cancelled => "CANCELLED",
        OperationState::FailedPermanent => "FAILED_PERMANENT",
    }
}

pub(crate) fn operation_state_from_sql(value: &str) -> OperationState {
    match value {
        "PENDING" => OperationState::Pending,
        "RUNNING" => OperationState::Running,
        "WAITING_RETRY" => OperationState::WaitingRetry,
        "WAITING_NETWORK" => OperationState::WaitingNetwork,
        "WAITING_AUTHENTICATION" => OperationState::WaitingAuthentication,
        "BLOCKED_BY_CONFLICT" => OperationState::BlockedByConflict,
        "COMPLETED" => OperationState::Completed,
        "CANCELLED" => OperationState::Cancelled,
        _ => OperationState::FailedPermanent,
    }
}

#[derive(Debug, Clone)]
pub struct IndexedItem {
    pub item_id: ItemId,
    pub remote_item_id: Option<String>,
    pub parent_item_id: Option<ItemId>,
    pub name: String,
    pub kind: ItemKind,
    /// Tamanho efetivo: local (`local_states.local_size_bytes`) quando o
    /// item tem conteúdo dirty, remoto caso contrário (SPEC §16.1 — a
    /// aplicação enxerga o conteúdo local sem esperar upload).
    pub size_bytes: u64,
    pub remote_version: Option<String>,
    pub children_loaded: bool,
    pub remote_modified_at_unix: Option<i64>,
    /// `local_states.sync_state` (`DIRTY`, `CLEAN`, ...) — `None` quando o
    /// item nunca teve conteúdo local materializado.
    pub sync_state: Option<String>,
    /// `items.source_layer` — `REMOTE`, `LOCAL` (criado localmente, ainda
    /// será sincronizado) ou `LOCAL_ONLY` (T4-05: excluído da sincronização
    /// pela avaliação de `nexofs-ignore`, vive só no overlay, nunca gera
    /// operação remota).
    pub source_layer: String,
}

pub(crate) fn item_kind_to_sql(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Directory => "DIRECTORY",
        ItemKind::File => "FILE",
    }
}

pub(crate) fn item_kind_from_sql(value: &str) -> ItemKind {
    match value {
        "DIRECTORY" => ItemKind::Directory,
        _ => ItemKind::File,
    }
}
