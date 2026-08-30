//! Resolução completa de conflitos (T4-12/T4-13, SPEC §18.4). A detecção
//! (T3-08) vive em `core/conflicts.rs`; aqui só o que acontece depois que o
//! usuário decide o que fazer com um conflito já registrado.

use super::SyncCore;
use crate::error::SyncError;
use crate::model::IndexedItem;
use crate::queries::{now_unix, parse_item_id};
use nexofs_conflicts::{conflict_resolution_to_sql, conflict_type_from_sql, generate_keep_both_name};
use nexofs_domain::states::{ConflictResolution, ConflictType};
use nexofs_domain::{ConflictId, ItemId};
use nexofs_provider_api::ItemKind;
use rusqlite::{params, OptionalExtension};

/// Uma linha de `conflicts`, o suficiente para a UI/CLI listar e para a
/// resolução decidir o que fazer (T4-03/T4-12).
#[derive(Debug, Clone)]
pub struct ConflictSummary {
    pub conflict_id: ConflictId,
    pub item_id: ItemId,
    pub conflict_type: ConflictType,
    pub state: String,
    pub detected_at: i64,
}

impl SyncCore {
    /// T4-03/diagnóstico: conflitos ainda não resolvidos deste namespace
    /// (inclui os adiados via `DismissTemporarily` — T4-13, continuam
    /// `OPEN` até uma resolução de verdade).
    pub async fn list_conflicts(&self) -> Result<Vec<ConflictSummary>, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let rows: Vec<(String, String, String, String, i64)> = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT conflict_id, item_id, conflict_type, state, detected_at FROM conflicts WHERE namespace_id = ?1 AND state = 'OPEN' ORDER BY detected_at ASC",
                )?;
                let rows = stmt.query_map([namespace_id_s], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|(conflict_id, item_id, conflict_type, state, detected_at)| {
                Some(ConflictSummary {
                    conflict_id: ConflictId(uuid::Uuid::parse_str(&conflict_id).ok()?),
                    item_id: parse_item_id(&item_id),
                    conflict_type: conflict_type_from_sql(&conflict_type)?,
                    state,
                    detected_at,
                })
            })
            .collect())
    }

    async fn open_conflict(&self, conflict_id: ConflictId) -> Result<Option<(ItemId, ConflictType)>, SyncError> {
        let conflict_id_s = conflict_id.to_string();
        let row: Option<(String, String)> = self
            .store
            .read(move |conn| {
                conn.query_row(
                    "SELECT item_id, conflict_type FROM conflicts WHERE conflict_id = ?1 AND state = 'OPEN'",
                    [conflict_id_s],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
            })
            .await?;
        Ok(row.and_then(|(item_id, conflict_type)| Some((parse_item_id(&item_id), conflict_type_from_sql(&conflict_type)?))))
    }

    async fn mark_conflict_resolved(&self, conflict_id: ConflictId, resolution: ConflictResolution) -> Result<(), SyncError> {
        let conflict_id_s = conflict_id.to_string();
        let resolution_s = conflict_resolution_to_sql(resolution);
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE conflicts SET state = 'RESOLVED', resolution = ?1, resolved_at = ?2 WHERE conflict_id = ?3",
                    params![resolution_s, now, conflict_id_s],
                )
            })
            .await?;
        Ok(())
    }

    /// T4-13: "conflito adiado" — o usuário viu e decidiu tratar depois.
    /// Continua `OPEN` (recuperável após reinício, não duplica se o mesmo
    /// erro acontecer de novo — `record_conflict` já é idempotente por
    /// item) e o item continua protegido de eviction (`sync_state =
    /// 'CONFLICT'` nunca muda aqui).
    pub async fn dismiss_conflict(&self, conflict_id: ConflictId) -> Result<(), SyncError> {
        let conflict_id_s = conflict_id.to_string();
        let resolution_s = conflict_resolution_to_sql(ConflictResolution::DismissTemporarily);
        self.store
            .write(move |tx| tx.execute("UPDATE conflicts SET resolution = ?1 WHERE conflict_id = ?2 AND state = 'OPEN'", params![resolution_s, conflict_id_s]))
            .await?;
        Ok(())
    }

    /// T4-12: aplica uma resolução a um conflito ainda aberto. Cobre os
    /// três tipos detectados desde T3-08; os demais tipos de `ConflictType`
    /// (colisão de nome/case, pai apagado, ...) dependem de mecanismos que
    /// ainda não os produzem (T4-06 em diante), então não têm resolução
    /// aqui ainda.
    pub async fn resolve_conflict(&self, conflict_id: ConflictId, resolution: ConflictResolution) -> Result<(), SyncError> {
        let Some((item_id, conflict_type)) = self.open_conflict(conflict_id).await? else {
            return Err(SyncError::NotFound);
        };

        if resolution == ConflictResolution::DismissTemporarily {
            return self.dismiss_conflict(conflict_id).await;
        }

        match conflict_type {
            ConflictType::ContentChangedBothSides => self.resolve_content_changed_both_sides(item_id, resolution).await?,
            ConflictType::RemoteDeletedLocalModified => self.resolve_remote_deleted_local_modified(item_id, resolution).await?,
            ConflictType::LocalDeletedRemoteModified => self.resolve_local_deleted_remote_modified(item_id, resolution).await?,
            ConflictType::LocalOnlyRemoteCollision => self.resolve_local_only_remote_collision(item_id, resolution).await?,
            _ => return Err(SyncError::InvalidOperation("resolução ainda não implementada para este tipo de conflito")),
        }

        self.mark_conflict_resolved(conflict_id, resolution).await?;
        self.event_bus
            .publish(crate::events::SyncEvent::ConflictResolved { namespace_id: self.ctx.namespace_id, conflict_id });
        Ok(())
    }

    async fn resolve_content_changed_both_sides(&self, item_id: ItemId, resolution: ConflictResolution) -> Result<(), SyncError> {
        let item = self.get_item(item_id).await?.ok_or(SyncError::NotFound)?;
        match resolution {
            ConflictResolution::KeepLocal => self.force_reupload_over_remote_change(&item).await,
            ConflictResolution::KeepRemote => self.discard_local_edit_and_rehydrate(&item).await,
            ConflictResolution::KeepBoth | ConflictResolution::SaveLocalElsewhere => {
                self.split_dirty_content_into_new_sibling(&item).await?;
                self.discard_local_edit_and_rehydrate(&item).await
            }
            ConflictResolution::DismissTemporarily => unreachable!("tratado em resolve_conflict antes de chegar aqui"),
        }
    }

    /// "Local vence": descobre a versão remota REAL agora (não a base
    /// obsoleta que causou o conflito) e reenvia o conteúdo local usando-a
    /// como `If-Match` — o dispatcher sozinho nunca conseguiria passar
    /// disso, porque `local_states.base_remote_version` continua
    /// apontando para a versão antiga até alguém a atualizar de propósito.
    async fn force_reupload_over_remote_change(&self, item: &IndexedItem) -> Result<(), SyncError> {
        // `ContentChangedBothSides` também é o `ConflictType` usado para um
        // eTag desatualizado num MOVE_ITEM/RENAME_ITEM (o Graph não
        // distingue "PATCH de posição com eTag velho" de "upload de
        // conteúdo com eTag velho") — quando não existe arquivo dirty,
        // não há bytes para reenviar; "manter local" aqui significa
        // reencaminhar o move/rename já aplicado ao índice, não um upload.
        // Bug real encontrado em produção: sem este desvio, isto
        // enfileirava um UploadFile que nunca achava conteúdo dirty (já
        // consumido por um upload anterior, ou nunca existiu porque o
        // conflito original nem era de conteúdo) e ficava tentando de novo
        // para sempre (T6-09/journal nunca detectava isso como permanente).
        if !self.cache.dirty_path(&item.item_id.to_string()).is_file() {
            return self.retry_blocked_move_or_rename(item).await;
        }
        let fresh_version = match &item.remote_item_id {
            Some(remote_item_id) => self.fetch_current_remote_item(remote_item_id.clone()).await?.and_then(|r| r.remote_version),
            None => None,
        };
        let item_id_s = item.item_id.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                // `local_version` PRECISA avançar aqui: a operação de
                // upload original que bateu no conflito ainda existe em
                // `operations`, `BLOCKED_BY_CONFLICT`, sob a chave de
                // idempotência antiga (que embute o `local_version` de
                // então). Sem avançar, `stabilize_upload` geraria a MESMA
                // chave, e `enqueue_operation` (que só atualiza uma chave
                // já existente quando ela está `PENDING`) devolveria a
                // operação velha intocada — `base_remote_version` novo
                // nunca chegaria a ela, e ela nunca sairia de
                // `BLOCKED_BY_CONFLICT`. Bug real encontrado pelo teste
                // desta resolução.
                tx.execute(
                    "UPDATE local_states SET sync_state = 'DIRTY', base_remote_version = ?1, local_version = local_version + 1, updated_at = ?2 WHERE item_id = ?3",
                    params![fresh_version, now, item_id_s],
                )
            })
            .await?;
        self.stabilize_upload(item.item_id).await
    }

    /// Reencaminha o `MOVE_ITEM`/`RENAME_ITEM` bloqueado deste item com uma
    /// `base_remote_version` atual, em vez de forçar um upload que não tem
    /// conteúdo dirty para enviar (ver `force_reupload_over_remote_change`).
    /// O índice local (`items.parent_item_id`/`name`) já reflete a posição
    /// final desejada — a mesma premissa de `dispatch_move`. Se houver mais
    /// de uma operação bloqueada para o item (ex.: um MOVE_ITEM seguido de
    /// um RENAME_ITEM antes do primeiro ser consumido), só a mais recente é
    /// reencaminhada; as demais são canceladas para não ficarem bloqueadas
    /// para sempre. `Ok(())` mesmo sem nenhuma operação bloqueada — não
    /// deveria acontecer, mas não é motivo para a resolução do conflito
    /// falhar.
    async fn retry_blocked_move_or_rename(&self, item: &IndexedItem) -> Result<(), SyncError> {
        let fresh_version = match &item.remote_item_id {
            Some(remote_item_id) => self.fetch_current_remote_item(remote_item_id.clone()).await?.and_then(|r| r.remote_version),
            None => None,
        };
        let item_id_s = item.item_id.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                let mut stmt = tx.prepare(
                    "SELECT operation_id FROM operations WHERE item_id = ?1 AND state = 'BLOCKED_BY_CONFLICT' AND operation_type IN ('MOVE_ITEM','RENAME_ITEM') ORDER BY updated_at DESC",
                )?;
                let blocked_ids: Vec<String> = stmt.query_map([&item_id_s], |row| row.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                drop(stmt);
                let Some((newest, older)) = blocked_ids.split_first() else {
                    return Ok(());
                };
                tx.execute(
                    "UPDATE operations SET state = 'PENDING', base_remote_version = ?1, next_attempt_at = NULL, updated_at = ?2 WHERE operation_id = ?3",
                    params![fresh_version, now, newest],
                )?;
                for op_id in older {
                    tx.execute("UPDATE operations SET state = 'CANCELLED', updated_at = ?1 WHERE operation_id = ?2", params![now, op_id])?;
                }
                // Sem isto o item ficava preso em `sync_state = 'CONFLICT'`
                // para sempre: só quem sabe tirar um item desse estado hoje
                // é a conclusão de um upload (`apply_uploaded_item`) — um
                // move/rename reencaminhado nunca passa por ali.
                tx.execute("UPDATE local_states SET sync_state = 'CLEAN', error_message = NULL, updated_at = ?1 WHERE item_id = ?2", params![now, item_id_s])?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// "Remoto vence": descarta o conteúdo dirty local e força uma
    /// rehidratação de verdade a partir do remoto atual — precisa jogar
    /// fora tanto `dirty/` quanto qualquer `clean/` cacheado (a versão
    /// limpa antiga não é a versão remota nova).
    async fn discard_local_edit_and_rehydrate(&self, item: &IndexedItem) -> Result<(), SyncError> {
        let cache_object_id = item.item_id.to_string();
        let _ = self.cache.remove_dirty(&cache_object_id);
        let _ = self.cache.remove(&cache_object_id);

        if let Some(remote_item_id) = &item.remote_item_id {
            if let Some(fresh) = self.fetch_current_remote_item(remote_item_id.clone()).await? {
                let item_id_s = item.item_id.to_string();
                let version = fresh.remote_version.clone();
                let content_version = fresh.remote_content_version.clone();
                let size = fresh.size_bytes as i64;
                let now = now_unix();
                self.store
                    .write(move |tx| {
                        tx.execute(
                            "UPDATE items SET remote_version = ?1, remote_content_version = ?2, size_bytes = ?3, updated_at = ?4 WHERE item_id = ?5",
                            params![version, content_version, size, now, item_id_s],
                        )
                    })
                    .await?;
            }
        }

        let item_id_s = item.item_id.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE local_states SET hydration_state = 'EVICTED', sync_state = 'CLEAN', cache_object_id = NULL, local_size_bytes = NULL, dirty_since = NULL, updated_at = ?1 WHERE item_id = ?2",
                    params![now, item_id_s],
                )
            })
            .await?;
        Ok(())
    }

    /// "Manter os dois"/"salvar em outro lugar": o conteúdo dirty vira um
    /// arquivo novo, irmão do original, com nome único (SPEC §18.4). O item
    /// novo segue o fluxo normal de criação local — será enviado como
    /// qualquer outro arquivo novo, sem envolvimento do journal aqui.
    async fn split_dirty_content_into_new_sibling(&self, item: &IndexedItem) -> Result<(), SyncError> {
        let Some(parent_item_id) = item.parent_item_id else {
            return Err(SyncError::InvalidOperation("item sem pai não pode gerar uma cópia irmã"));
        };
        let cache_object_id = item.item_id.to_string();
        let dirty_path = self.cache.dirty_path(&cache_object_id);
        if !dirty_path.is_file() {
            return Ok(());
        }

        let siblings = self.list_children(parent_item_id).await?;
        let existing_names: std::collections::HashSet<String> = siblings.iter().map(|s| s.name.clone()).collect();
        let new_name = generate_keep_both_name(&item.name, chrono::Utc::now(), &existing_names);

        let new_item_id = self.create_local_item(parent_item_id, &new_name, ItemKind::File).await?;
        let new_path = self.begin_write(new_item_id).await?;
        tokio::fs::copy(&dirty_path, &new_path).await?;
        let size = tokio::fs::metadata(&new_path).await?.len();
        self.update_local_size(new_item_id, size).await?;
        self.stabilize_upload(new_item_id).await
    }

    async fn resolve_remote_deleted_local_modified(&self, item_id: ItemId, resolution: ConflictResolution) -> Result<(), SyncError> {
        match resolution {
            // Sem remoto para "manter" ou "dividir" — as três opções não
            // triviais convergem para o mesmo resultado: recriar do zero a
            // partir do conteúdo local, que é a única cópia que existe.
            ConflictResolution::KeepLocal | ConflictResolution::KeepBoth | ConflictResolution::SaveLocalElsewhere => {
                let item_id_s = item_id.to_string();
                let now = now_unix();
                // `local_version + 1` pela mesma razão de
                // `force_reupload_over_remote_change`: a operação original
                // que bateu em `NotFound` continua em `BLOCKED_BY_CONFLICT`
                // sob a chave de idempotência antiga.
                self.store
                    .write(move |tx| {
                        tx.execute(
                            "UPDATE local_states SET sync_state = 'DIRTY', local_version = local_version + 1, updated_at = ?1 WHERE item_id = ?2",
                            params![now, item_id_s],
                        )
                    })
                    .await?;
                self.stabilize_upload(item_id).await
            }
            ConflictResolution::KeepRemote => self.hard_delete_item(item_id).await,
            ConflictResolution::DismissTemporarily => unreachable!("tratado em resolve_conflict antes de chegar aqui"),
        }
    }

    async fn resolve_local_deleted_remote_modified(&self, item_id: ItemId, resolution: ConflictResolution) -> Result<(), SyncError> {
        let item = self.get_item(item_id).await?.ok_or(SyncError::NotFound)?;
        match resolution {
            ConflictResolution::KeepLocal => {
                // A intenção local (apagar) vence — descobre a versão
                // remota real e apaga com ela como base.
                if let Some(remote_item_id) = item.remote_item_id.clone() {
                    let fresh_version = self.fetch_current_remote_item(remote_item_id.clone()).await?.and_then(|r| r.remote_version);
                    self.delete_remote_item_now(remote_item_id, fresh_version).await?;
                }
                self.hard_delete_item(item_id).await
            }
            // Sem conteúdo dirty para "dividir" — a intenção local era
            // apagar, não editar; as três opções convergem para desistir da
            // exclusão e manter o item.
            ConflictResolution::KeepRemote | ConflictResolution::KeepBoth | ConflictResolution::SaveLocalElsewhere => {
                let item_id_s = item_id.to_string();
                let now = now_unix();
                self.store
                    .write(move |tx| tx.execute("UPDATE local_states SET sync_state = 'CLEAN', updated_at = ?1 WHERE item_id = ?2", params![now, item_id_s]))
                    .await?;
                Ok(())
            }
            ConflictResolution::DismissTemporarily => unreachable!("tratado em resolve_conflict antes de chegar aqui"),
        }
    }

    /// `LocalOnlyRemoteCollision` (detectada em `upsert_item`, `core/mod.rs`):
    /// um item criado localmente ainda não enviado (`remote_item_id IS
    /// NULL`) ocupa o mesmo `(pasta, nome)` que um item remoto recém-visto.
    /// O item remoto em si nunca chegou a ser indexado — `upsert_item`
    /// pulou-o inteiro ao detectar a colisão — então não há uma "cópia
    /// remota" para comparar ou mesclar aqui, diferente dos outros tipos de
    /// conflito.
    async fn resolve_local_only_remote_collision(&self, item_id: ItemId, resolution: ConflictResolution) -> Result<(), SyncError> {
        match resolution {
            // "Remoto vence": descarta o item local que nunca foi enviado —
            // a próxima relistagem da pasta pai insere o item remoto
            // normalmente, sem mais nada disputando o nome.
            ConflictResolution::KeepRemote => self.hard_delete_item(item_id).await,
            // "Local vence"/"manter os dois": como o remoto nem chegou a
            // ser indexado, não existe operação de rename remoto a fazer —
            // basta tirar o item local do nome disputado (mesmo padrão de
            // `generate_keep_both_name` usado em
            // `split_dirty_content_into_new_sibling`). O nome original fica
            // livre para a próxima relistagem adotar o item remoto.
            ConflictResolution::KeepLocal | ConflictResolution::KeepBoth | ConflictResolution::SaveLocalElsewhere => {
                self.rename_local_only_item_out_of_the_way(item_id).await
            }
            ConflictResolution::DismissTemporarily => unreachable!("tratado em resolve_conflict antes de chegar aqui"),
        }
    }

    async fn rename_local_only_item_out_of_the_way(&self, item_id: ItemId) -> Result<(), SyncError> {
        let item = self.get_item(item_id).await?.ok_or(SyncError::NotFound)?;
        let Some(parent_item_id) = item.parent_item_id else {
            return Err(SyncError::InvalidOperation("item sem pai não pode ser renomeado para sair do caminho"));
        };

        let siblings = self.list_children(parent_item_id).await?;
        let existing_names: std::collections::HashSet<String> = siblings.iter().map(|s| s.name.clone()).collect();
        let new_name = generate_keep_both_name(&item.name, chrono::Utc::now(), &existing_names);
        let normalized = new_name.to_lowercase();

        let item_id_s = item_id.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE items SET name = ?1, normalized_name = ?2, updated_at = ?3 WHERE item_id = ?4",
                    params![new_name, normalized, now, item_id_s],
                )
            })
            .await?;

        // `record_conflict` sobrescreveu `local_states.sync_state` para
        // `CONFLICT` incondicionalmente — restaura o estado real do item
        // (a mesma distinção `LOCAL_ONLY` vs. conteúdo pendente de
        // `create_local_item`) antes de deixar o dispatcher retomar a
        // operação pendente, agora sob o nome novo.
        if item.sync_state.as_deref() == Some("CONFLICT") {
            let restored_state = if item.source_layer == "LOCAL_ONLY" { "LOCAL_ONLY" } else { "DIRTY" };
            let item_id_s = item_id.to_string();
            let now = now_unix();
            self.store
                .write(move |tx| {
                    tx.execute(
                        "UPDATE local_states SET sync_state = ?1, error_message = NULL, updated_at = ?2 WHERE item_id = ?3",
                        params![restored_state, now, item_id_s],
                    )
                })
                .await?;
        }

        // Sem efeito se o item não estiver `Dirty` (diretórios e itens
        // `LOCAL_ONLY` caem no early-return de `stabilize_upload`) — a
        // `CreateDirectory` já enfileirada por `create_local_item` lê
        // `item.name` direto de `items` no momento do dispatch
        // (`dispatch_create_directory`), então o rename acima já basta
        // para ela também.
        self.stabilize_upload(item_id).await
    }
}
