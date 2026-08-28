//! Persistência de conflitos estruturados (T3-08, SPEC §18). A
//! classificação (que tipo de conflito um erro do provedor representa) vive
//! em `nexofs-conflicts`, que não conhece SQLite; aqui só a gravação em
//! `conflicts` e a transição de estado do item associado.

use super::SyncCore;
use crate::error::SyncError;
use crate::queries::{now_unix, parse_item_id};
use nexofs_conflicts::conflict_type_to_sql;
use nexofs_domain::states::ConflictType;
use nexofs_domain::ItemId;
use rusqlite::{params, OptionalExtension};

impl SyncCore {
    /// Registra um conflito estruturado e marca `local_states.sync_state =
    /// 'CONFLICT'` — preserva o snapshot dirty existente (SPEC §18.3: nada
    /// aqui apaga ou sobrescreve conteúdo local) e sinaliza para o resto do
    /// núcleo (elegibilidade de eviction em T4-11, `/v1/conflicts` na Fase 5)
    /// que este item precisa de decisão do usuário. Idempotente: um item já
    /// com um conflito `OPEN` não ganha um segundo registro — o operador
    /// resolve um de cada vez (resolução completa fica para T4-12).
    pub(crate) async fn record_conflict(
        &self,
        item_id: ItemId,
        conflict_type: ConflictType,
        message: &str,
    ) -> Result<(), SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let item_id_s = item_id.to_string();
        let conflict_type_s = conflict_type_to_sql(conflict_type);
        let message_owned = message.to_string();
        let now = now_unix();
        let conflict_id = uuid::Uuid::new_v4();
        let conflict_id_s = conflict_id.to_string();

        let recorded = self
            .store
            .write(move |tx| {
                let already_open: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM conflicts WHERE item_id = ?1 AND state = 'OPEN'",
                        [&item_id_s],
                        |row| row.get(0),
                    )
                    .optional()?;
                if already_open.is_some() {
                    return Ok(false);
                }

                let local: Option<(i64, Option<String>)> = tx
                    .query_row(
                        "SELECT local_version, base_remote_version FROM local_states WHERE item_id = ?1",
                        [&item_id_s],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let (local_version, base_remote_version) = local.unwrap_or((0, None));
                let current_remote_version: Option<String> = tx
                    .query_row("SELECT remote_version FROM items WHERE item_id = ?1", [&item_id_s], |row| row.get(0))
                    .optional()?
                    .unwrap_or(None);

                tx.execute(
                    "INSERT INTO conflicts (conflict_id, namespace_id, item_id, conflict_type, state, local_version, base_remote_version, current_remote_version, detected_at) \
                     VALUES (?1, ?2, ?3, ?4, 'OPEN', ?5, ?6, ?7, ?8)",
                    params![conflict_id_s, namespace_id_s, item_id_s, conflict_type_s, local_version, base_remote_version, current_remote_version, now],
                )?;
                tx.execute(
                    "UPDATE local_states SET sync_state = 'CONFLICT', error_message = ?1, updated_at = ?2 WHERE item_id = ?3",
                    params![message_owned, now, item_id_s],
                )?;
                Ok(true)
            })
            .await?;

        if recorded {
            tracing::warn!(%item_id, ?conflict_type, message, "conflito detectado — snapshot local preservado");
            self.event_bus.publish(crate::events::SyncEvent::ConflictCreated {
                namespace_id: self.ctx.namespace_id,
                conflict_id: nexofs_domain::ConflictId(conflict_id),
                item_id,
            });
        }
        Ok(())
    }

    /// Reconciliação de partida: bug real encontrado em produção — em algum
    /// momento antes do registro de conflitos existir para `DeleteItem`,
    /// uma exclusão local podia ficar `BLOCKED_BY_CONFLICT` sem nenhuma
    /// linha correspondente em `conflicts`. Sem o registro, a aba
    /// "Conflitos" não tinha como mostrar nada, e o item ficava escondido
    /// da listagem local (`sync_state = 'DELETED_LOCALLY'`) para sempre —
    /// sem forma de o usuário decidir "apagar de verdade" ou "restaurar".
    /// Só cobre `DeleteItem`: é o único tipo de operação cujo
    /// `ConflictType` correspondente (`LocalDeletedRemoteModified`) é
    /// determinístico sem precisar reconstruir o `ProviderErrorKind`
    /// original a partir da mensagem de erro salva. Retorna quantos
    /// conflitos foram reconciliados, para log de diagnóstico.
    pub async fn backfill_missing_delete_conflicts(&self) -> Result<u64, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let orphaned_item_ids: Vec<String> = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT o.item_id FROM operations o \
                     WHERE o.namespace_id = ?1 AND o.state = 'BLOCKED_BY_CONFLICT' AND o.operation_type = 'DELETE_ITEM' \
                     AND o.item_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM conflicts c WHERE c.item_id = o.item_id)",
                )?;
                let rows = stmt.query_map([&namespace_id_s], |row| row.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;

        for item_id_s in &orphaned_item_ids {
            self.record_conflict(
                parse_item_id(item_id_s),
                ConflictType::LocalDeletedRemoteModified,
                "conflito reconciliado na inicialização — a operação já estava bloqueada, mas nunca tinha sido registrada",
            )
            .await?;
        }
        Ok(orphaned_item_ids.len() as u64)
    }
}
