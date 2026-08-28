use crate::model::{item_kind_from_sql, operation_state_from_sql, operation_type_from_sql, IndexedItem, QueuedOperation};
use nexofs_domain::{ItemId, OperationId};
use rusqlite::Row;

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("relógio do sistema não pode estar antes de 1970")
        .as_secs() as i64
}

pub(crate) fn parse_item_id(s: &str) -> ItemId {
    ItemId(uuid::Uuid::parse_str(s).expect("item_id armazenado é sempre um UUID válido gerado por nós"))
}

pub(crate) fn parse_operation_id(s: &str) -> OperationId {
    OperationId(uuid::Uuid::parse_str(s).expect("operation_id armazenado é sempre um UUID válido gerado por nós"))
}

pub(crate) const OPERATION_COLUMNS: &str =
    "operation_id, item_id, operation_type, state, priority, idempotency_key, attempt_count, base_remote_version";

pub(crate) fn row_to_operation(row: &Row<'_>) -> rusqlite::Result<QueuedOperation> {
    let operation_id: String = row.get(0)?;
    let item_id: Option<String> = row.get(1)?;
    let operation_type: String = row.get(2)?;
    let state: String = row.get(3)?;
    let priority: i64 = row.get(4)?;
    let idempotency_key: String = row.get(5)?;
    let attempt_count: i64 = row.get(6)?;
    let base_remote_version: Option<String> = row.get(7)?;

    Ok(QueuedOperation {
        operation_id: parse_operation_id(&operation_id),
        item_id: item_id.as_deref().map(parse_item_id),
        operation_type: operation_type_from_sql(&operation_type),
        state: operation_state_from_sql(&state),
        priority: priority.clamp(0, u8::MAX as i64) as u8,
        attempt_count: attempt_count.max(0) as u32,
        idempotency_key,
        base_remote_version,
    })
}

/// Colunas de `items` com `LEFT JOIN local_states` embutido — todo lugar que
/// lê itens precisa enxergar o tamanho e o `sync_state` locais quando
/// existirem (SPEC §16.1), então em vez de repetir o `JOIN` em cada query,
/// `FROM_ITEMS` (abaixo) já vem pronto para ser usado no lugar de `FROM
/// items` puro. Referências a `item_id`/`remote_state` nas cláusulas `WHERE`
/// de quem usa isto DEVEM ser qualificadas como `items.<coluna>` — o nome é
/// ambíguo entre as duas tabelas depois do `JOIN`.
pub(crate) const ITEM_COLUMNS: &str = "items.item_id, items.remote_item_id, items.parent_item_id, items.name, items.item_type, \
     COALESCE(local_states.local_size_bytes, items.size_bytes), items.remote_version, items.children_state, items.remote_modified_at, local_states.sync_state, items.source_layer";

pub(crate) const FROM_ITEMS: &str = "items LEFT JOIN local_states ON local_states.item_id = items.item_id";

/// Exclui itens apagados localmente (mas ainda não confirmados remotamente)
/// da leitura — combine com `remote_state <> 'DELETED'` no `WHERE` de quem
/// lista/busca itens visíveis.
pub(crate) const NOT_DELETED_LOCALLY: &str = "(local_states.sync_state IS NULL OR local_states.sync_state <> 'DELETED_LOCALLY')";

pub(crate) fn row_to_item(row: &Row<'_>) -> rusqlite::Result<IndexedItem> {
    let item_id: String = row.get(0)?;
    let remote_item_id: Option<String> = row.get(1)?;
    let parent_item_id: Option<String> = row.get(2)?;
    let name: String = row.get(3)?;
    let item_type: String = row.get(4)?;
    let size_bytes: i64 = row.get(5)?;
    let remote_version: Option<String> = row.get(6)?;
    let children_state: String = row.get(7)?;
    let remote_modified_at: Option<i64> = row.get(8)?;
    let sync_state: Option<String> = row.get(9)?;
    let source_layer: String = row.get(10)?;

    Ok(IndexedItem {
        item_id: parse_item_id(&item_id),
        remote_item_id,
        parent_item_id: parent_item_id.as_deref().map(parse_item_id),
        name,
        kind: item_kind_from_sql(&item_type),
        size_bytes: size_bytes.max(0) as u64,
        remote_version,
        children_loaded: children_state == "LOADED",
        remote_modified_at_unix: remote_modified_at,
        sync_state,
        source_layer,
    })
}
