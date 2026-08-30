//! Journal de operações remotas. SPEC §13.

use super::SyncCore;
use crate::error::SyncError;
use crate::model::{operation_state_to_sql, operation_type_to_sql, QueuedOperation};
use crate::queries::{now_unix, parse_operation_id, row_to_operation, OPERATION_COLUMNS};
use nexofs_domain::states::{OperationState, OperationType};
use nexofs_domain::{ItemId, OperationId};
use rusqlite::{params, OptionalExtension};

/// Rede de segurança para `WaitingNetwork` — a reconexão normalmente já
/// libera a operação bem antes disso via `wake_waiting_network_operations`
/// (T3-09); este valor só cobre o caso de a reconexão real acontecer sem
/// nenhuma chamada governada ser feita nesse meio tempo.
const WAITING_NETWORK_POLL_SECS: i64 = 30;

/// Filtros opcionais de `list_operations_page` — `None` em qualquer campo
/// significa "não filtrar por isto".
#[derive(Debug, Clone, Default)]
pub struct OperationsFilter {
    pub state: Option<OperationState>,
    pub operation_type: Option<OperationType>,
    /// Substring (case-insensitive para ASCII, como o `LIKE` padrão do
    /// SQLite) do nome do item — não do caminho completo, que exigiria uma
    /// CTE recursiva só para filtrar.
    pub search: Option<String>,
}

pub struct OperationsPage {
    pub operations: Vec<QueuedOperation>,
    /// Total sob os filtros pedidos (incluindo `state`) — pode ser maior
    /// que `operations.len()` quando a página não cobre tudo.
    pub total: u64,
    /// Total de `FAILED_PERMANENT` sob `operation_type`/`search`, mas
    /// ignorando `state` de propósito — ver doc de `list_operations_page`.
    pub total_failed: u64,
}

impl SyncCore {
    /// Enfileira uma operação remota com `idempotency_key` estável (SPEC
    /// §13.3). Uma chave já existente e ainda `Pending` tem seu payload
    /// atualizado em vez de gerar uma segunda linha — é isso que implementa,
    /// de forma genérica, a coalescência de §13.4 ("várias escritas → um
    /// upload final", "múltiplos renames → nome final"): quem chama escolhe
    /// a chave (com ou sem `local_version` embutido) conforme a operação
    /// deva colapsar por geração de conteúdo (upload/delete) ou sempre para
    /// a última intenção, independente de quantas vezes for chamada
    /// (rename/move). Uma chave já `Running`/`BlockedByConflict`/`Waiting*`
    /// nunca é perturbada — a operação em voo (ou aguardando o usuário)
    /// prevalece, e o chamador vê o mesmo `operation_id` de antes.
    ///
    /// Bug real de produção: uma chave num estado TERMINAL (`Completed`,
    /// `Cancelled`, `FailedPermanent`) era tratada do mesmo jeito — sem
    /// nunca reescrever a linha, mas o `UNIQUE(idempotency_key)` também
    /// impedia inserir uma segunda. Como `rename_local_item` usa uma chave
    /// sem `local_version` (de propósito, para colapsar múltiplos renames
    /// antes do dispatch), mover o MESMO item pela segunda vez — mesmo
    /// muito depois, com a primeira mudança já concluída — reaproveitava
    /// essa chave e caía direto nesse limbo: o índice local mudava (quem
    /// chama nunca olha o retorno), mas nenhuma chamada nova ao provedor
    /// era feita, silenciosamente, para sempre. Uma chave terminal agora é
    /// reciclada como uma operação nova.
    pub(crate) async fn enqueue_operation(
        &self,
        item_id: Option<ItemId>,
        operation_type: OperationType,
        idempotency_key: String,
        priority: u8,
        base_remote_version: Option<String>,
        payload_json: String,
    ) -> Result<OperationId, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let item_id_s = item_id.map(|id| id.to_string());
        let operation_type_s = operation_type_to_sql(operation_type);
        let now = now_unix();

        let operation_id = self
            .store
            .write(move |tx| {
                let existing: Option<(String, String)> = tx
                    .query_row(
                        "SELECT operation_id, state FROM operations WHERE idempotency_key = ?1",
                        [&idempotency_key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;

                if let Some((id, state)) = existing {
                    if state == "PENDING" {
                        tx.execute(
                            "UPDATE operations SET payload_json = ?1, base_remote_version = ?2, updated_at = ?3 WHERE operation_id = ?4",
                            params![payload_json, base_remote_version, now, id],
                        )?;
                    } else if matches!(state.as_str(), "COMPLETED" | "CANCELLED" | "FAILED_PERMANENT") {
                        // Recicla a linha como uma operação nova — reseta
                        // tudo que pertencia à tentativa anterior para não
                        // herdar `attempt_count`/erro de um ciclo de vida
                        // que já tinha terminado.
                        tx.execute(
                            "UPDATE operations SET operation_type = ?1, state = 'PENDING', priority = ?2, payload_json = ?3, base_remote_version = ?4, attempt_count = 0, next_attempt_at = NULL, last_error_message = NULL, created_at = ?5, updated_at = ?5 WHERE operation_id = ?6",
                            params![operation_type_s, priority, payload_json, base_remote_version, now, id],
                        )?;
                    }
                    return Ok(parse_operation_id(&id));
                }

                let new_id = OperationId::new();
                let new_id_s = new_id.to_string();
                tx.execute(
                    "INSERT INTO operations (operation_id, namespace_id, item_id, operation_type, state, priority, idempotency_key, base_remote_version, payload_json, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, 'PENDING', ?5, ?6, ?7, ?8, ?9, ?9)",
                    params![new_id_s, namespace_id_s, item_id_s, operation_type_s, priority, idempotency_key, base_remote_version, payload_json, now],
                )?;
                Ok(new_id)
            })
            .await?;

        Ok(operation_id)
    }

    /// Cancela (remove) operações ainda `Pending` deste item — usado quando
    /// uma exclusão local torna um upload/create anterior obsoleto (SPEC
    /// §13.4 "create + delete antes do upload → cancelar ambos").
    pub(crate) async fn cancel_pending_operations_for_item(&self, item_id: ItemId) -> Result<(), SyncError> {
        let item_id_s = item_id.to_string();
        self.store
            .write(move |tx| tx.execute("DELETE FROM operations WHERE item_id = ?1 AND state = 'PENDING'", [item_id_s]))
            .await?;
        Ok(())
    }

    pub async fn mark_operation_running(&self, operation_id: OperationId) -> Result<(), SyncError> {
        self.update_operation_state(operation_id, OperationState::Running, None).await
    }

    pub async fn mark_operation_completed(&self, operation_id: OperationId) -> Result<(), SyncError> {
        self.update_operation_state(operation_id, OperationState::Completed, None).await
    }

    pub async fn mark_operation_cancelled(&self, operation_id: OperationId) -> Result<(), SyncError> {
        self.update_operation_state(operation_id, OperationState::Cancelled, None).await
    }

    /// `POST /v1/operations/{id}/cancel` (T5-01/SPEC §20.3): só operações
    /// ainda não em voo podem ser canceladas por comando manual — `Running`
    /// pode já ter efeito no remoto que cancelar aqui não desfaria, e um
    /// estado terminal já não representa nada para cancelar. Retorna se a
    /// operação existia e estava num estado cancelável.
    pub async fn cancel_operation(&self, operation_id: OperationId) -> Result<bool, SyncError> {
        let operation_id_s = operation_id.to_string();
        let state_s = operation_state_to_sql(OperationState::Cancelled);
        let now = now_unix();
        let updated = self
            .store
            .write(move |tx| {
                tx.execute(
                    "UPDATE operations SET state = ?1, updated_at = ?2 \
                     WHERE operation_id = ?3 AND state IN ('PENDING','WAITING_RETRY','WAITING_NETWORK','WAITING_AUTHENTICATION')",
                    params![state_s, now, operation_id_s],
                )
            })
            .await?;
        if updated > 0 {
            self.publish_operation_progress(operation_id, state_s).await;
        }
        Ok(updated > 0)
    }

    /// `POST /v1/operations/{id}/retry` (T5-01/SPEC §20.3): força o próximo
    /// tick do dispatcher a tentar de novo imediatamente, mesmo dentro do
    /// backoff atual — só faz sentido para operações hoje esperando (não
    /// para `Pending`, que já está na fila; nem para estados terminais).
    pub async fn retry_operation(&self, operation_id: OperationId) -> Result<bool, SyncError> {
        let operation_id_s = operation_id.to_string();
        let state_s = operation_state_to_sql(OperationState::Pending);
        let now = now_unix();
        let updated = self
            .store
            .write(move |tx| {
                tx.execute(
                    "UPDATE operations SET state = ?1, next_attempt_at = NULL, updated_at = ?2 \
                     WHERE operation_id = ?3 AND state IN ('WAITING_RETRY','WAITING_NETWORK','WAITING_AUTHENTICATION','FAILED_PERMANENT')",
                    params![state_s, now, operation_id_s],
                )
            })
            .await?;
        if updated > 0 {
            self.publish_operation_progress(operation_id, state_s).await;
        }
        Ok(updated > 0)
    }

    pub async fn mark_operation_failed_permanent(
        &self,
        operation_id: OperationId,
        error_message: &str,
    ) -> Result<(), SyncError> {
        self.update_operation_state(operation_id, OperationState::FailedPermanent, Some(error_message.to_string()))
            .await
    }

    /// Registra uma falha transitória (rede/rate limit/auth) sem consumir a
    /// operação — volta para `WaitingRetry` e soma uma tentativa, mantendo o
    /// conteúdo local intocado (SPEC §16.3 "auth/network/rate limit → retry
    /// sem perda local").
    pub async fn mark_operation_waiting_retry(
        &self,
        operation_id: OperationId,
        next_attempt_at_unix: i64,
        error_message: &str,
    ) -> Result<(), SyncError> {
        let operation_id_s = operation_id.to_string();
        let state_s = operation_state_to_sql(OperationState::WaitingRetry);
        let error_message = error_message.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE operations SET state = ?1, attempt_count = attempt_count + 1, next_attempt_at = ?2, last_error_message = ?3, updated_at = ?4 WHERE operation_id = ?5",
                    params![state_s, next_attempt_at_unix, error_message, now, operation_id_s],
                )
            })
            .await?;
        self.publish_operation_progress(operation_id, state_s).await;
        Ok(())
    }

    /// Registra uma falha de conectividade (`Network`/`Timeout`) sem
    /// consumir a operação, distinta de `mark_operation_waiting_retry`
    /// (T3-09/FR-OFF-005): o backoff aqui é só uma rede de segurança — a
    /// reconexão detectada por `execute_governed` (`core/connectivity.rs`)
    /// já libera esta operação imediatamente via
    /// `wake_waiting_network_operations`, sem esperar os 30s correrem.
    pub async fn mark_operation_waiting_network(&self, operation_id: OperationId, error_message: &str) -> Result<(), SyncError> {
        let operation_id_s = operation_id.to_string();
        let state_s = operation_state_to_sql(OperationState::WaitingNetwork);
        let error_message = error_message.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE operations SET state = ?1, attempt_count = attempt_count + 1, next_attempt_at = ?2, last_error_message = ?3, updated_at = ?4 WHERE operation_id = ?5",
                    params![state_s, now + WAITING_NETWORK_POLL_SECS, error_message, now, operation_id_s],
                )
            })
            .await?;
        self.publish_operation_progress(operation_id, state_s).await;
        Ok(())
    }

    /// Libera para o próximo tick do dispatcher toda operação
    /// `WaitingNetwork` deste namespace — chamado só na transição
    /// offline→online (`core/connectivity.rs`), nunca num timer, para não
    /// martelar o provedor enquanto a rede claramente ainda está fora.
    pub(crate) async fn wake_waiting_network_operations(&self) -> Result<(), SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE operations SET next_attempt_at = NULL WHERE namespace_id = ?1 AND state = 'WAITING_NETWORK'",
                    [namespace_id_s],
                )
            })
            .await?;
        Ok(())
    }

    async fn update_operation_state(
        &self,
        operation_id: OperationId,
        state: OperationState,
        error_message: Option<String>,
    ) -> Result<(), SyncError> {
        let operation_id_s = operation_id.to_string();
        let state_s = operation_state_to_sql(state);
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE operations SET state = ?1, last_error_message = COALESCE(?2, last_error_message), updated_at = ?3 WHERE operation_id = ?4",
                    params![state_s, error_message, now, operation_id_s],
                )
            })
            .await?;
        self.publish_operation_progress(operation_id, state_s).await;
        Ok(())
    }

    /// T5-02: ponto único de emissão de `OperationProgress` — chamado tanto
    /// por `update_operation_state` quanto pelas transições que gravam SQL
    /// diretamente (`mark_operation_waiting_retry`/`_network`, que também
    /// somam `attempt_count` e não passam por ali). T5-desktop: também
    /// resolve `operation_type`/nome/caminho do item aqui, num único lugar,
    /// em vez de cada chamador duplicar a mesma consulta — é o que a aba
    /// "Log" da UI precisa para mostrar "Enviado arquivo.txt em Documentos"
    /// em vez de só um `operation_id` opaco com um estado.
    pub(crate) async fn publish_operation_progress(&self, operation_id: OperationId, state: &str) {
        let operation_id_s = operation_id.to_string();
        let row: Option<(String, Option<String>)> = self
            .store
            .read(move |conn| {
                conn.query_row("SELECT operation_type, item_id FROM operations WHERE operation_id = ?1", [operation_id_s], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()
            })
            .await
            .ok()
            .flatten();

        let mut operation_type = None;
        let mut item_name = None;
        let mut item_path = None;
        if let Some((operation_type_s, item_id_s)) = row {
            operation_type = Some(operation_type_s);
            if let Some(item_id_s) = item_id_s {
                let item_id = crate::queries::parse_item_id(&item_id_s);
                if let Ok(Some(item)) = self.get_item(item_id).await {
                    item_name = Some(item.name);
                }
                item_path = self.item_relative_path(item_id).await.ok().map(|p| p.to_string_lossy().to_string());
            }
        }

        self.event_bus.publish(crate::events::SyncEvent::OperationProgress {
            namespace_id: self.ctx.namespace_id,
            operation_id,
            operation_type,
            state: state.to_string(),
            item_name,
            item_path,
        });
    }

    /// SPEC §13.5 pt.1: recuperação após reinício — operações que ficaram
    /// `Running` numa queda anterior (ex.: `kill -9` em pleno upload) não
    /// têm garantia de que a chamada remota completou ou não; a única opção
    /// segura é reabri-las como `Pending` para o dispatcher decidir de novo
    /// (o `idempotency_key` estável evita duplicar efeito no lado remoto
    /// quando o adaptador suportar upload condicional). Retorna quantas
    /// linhas foram recuperadas, para log de diagnóstico.
    pub async fn recover_running_operations(&self) -> Result<u64, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let now = now_unix();
        let recovered = self
            .store
            .write(move |tx| {
                tx.execute(
                    "UPDATE operations SET state = 'PENDING', updated_at = ?1 WHERE namespace_id = ?2 AND state = 'RUNNING'",
                    params![now, namespace_id_s],
                )
            })
            .await?;
        Ok(recovered as u64)
    }

    /// Operações ainda não concluídas deste namespace, em ordem de despacho
    /// (prioridade menor primeiro, depois FIFO) — usado pelo dispatcher
    /// (Fase 3, upload real) e por diagnóstico/`/v1/metrics`.
    pub async fn pending_operations(&self) -> Result<Vec<QueuedOperation>, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let query = format!(
            "SELECT {OPERATION_COLUMNS} FROM operations WHERE namespace_id = ?1 AND state IN ('PENDING','WAITING_RETRY','WAITING_NETWORK','WAITING_AUTHENTICATION') ORDER BY priority ASC, created_at ASC"
        );
        let rows = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(&query)?;
                let rows = stmt.query_map([namespace_id_s], row_to_operation)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;
        Ok(rows)
    }

    /// Operações que já desistiram de vez (`FailedPermanent`) deste
    /// namespace, mais recentes primeiro — nunca aparecem em
    /// `pending_operations()` (não são mais "trabalho em andamento" para o
    /// dispatcher/storm-detector), mas continuam precisando de atenção
    /// humana: reautenticação, nome inválido, erro real do provedor (ex.:
    /// `HTTP 411` visto em produção). Sem isto, uma falha permanente só
    /// existia no log do daemon — o usuário via o arquivo como "sincronizado
    /// há muito tempo" sem saber que ele nunca chegou a subir. Usado por
    /// `GET /v1/operations`, `/v1/metrics` e o pacote de diagnóstico.
    pub async fn failed_operations(&self) -> Result<Vec<QueuedOperation>, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let query = format!("SELECT {OPERATION_COLUMNS} FROM operations WHERE namespace_id = ?1 AND state = 'FAILED_PERMANENT' ORDER BY updated_at DESC");
        let rows = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(&query)?;
                let rows = stmt.query_map([namespace_id_s], row_to_operation)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;
        Ok(rows)
    }

    /// `GET /v1/operations` com paginação e filtros (T7-06): página pedida
    /// (`limit`/`offset`) de operações ainda não concluídas — o mesmo
    /// conjunto de `pending_operations()` + `failed_operations()`, mas como
    /// UMA query paginável, com `state`/`operation_type`/`search` (nome do
    /// item, `LIKE`) opcionais. `total`/`total_failed` continuam contando
    /// via `COUNT(*)` (sem resolver nome/caminho — o que é caro), então os
    /// indicadores da UI batem certo mesmo com a página atual truncada.
    /// `total_failed` ignora o filtro de `state` de propósito: é o que
    /// permite ao usuário ver quantos falharam de vez mesmo filtrando por
    /// outro estado, e clicar nesse número para trocar o filtro.
    pub async fn list_operations_page(&self, filter: OperationsFilter, limit: u32, offset: u32) -> Result<OperationsPage, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let state_s = filter.state.map(operation_state_to_sql);
        let operation_type_s = filter.operation_type.map(operation_type_to_sql);
        // Escapa `%`/`_` do termo digitado antes de envolver em `%...%` —
        // sem isto, um nome de arquivo real contendo esses caracteres (ex.:
        // "50%_final.docx") quebraria o `LIKE` de formas surpreendentes.
        let search_pattern = filter.search.as_deref().filter(|s| !s.trim().is_empty()).map(|s| {
            let escaped = s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            format!("%{escaped}%")
        });
        let limit = limit.clamp(1, 200) as i64;
        let offset = offset as i64;

        let count_query = "SELECT COUNT(*) FROM operations WHERE namespace_id = ?1 \
             AND state IN ('PENDING','WAITING_RETRY','WAITING_NETWORK','WAITING_AUTHENTICATION','FAILED_PERMANENT') \
             AND (?2 IS NULL OR state = ?2) \
             AND (?3 IS NULL OR operation_type = ?3) \
             AND (?4 IS NULL OR item_id IN (SELECT item_id FROM items WHERE namespace_id = ?1 AND name LIKE ?4 ESCAPE '\\'))";
        let failed_count_query = "SELECT COUNT(*) FROM operations WHERE namespace_id = ?1 \
             AND state = 'FAILED_PERMANENT' \
             AND (?3 IS NULL OR operation_type = ?3) \
             AND (?4 IS NULL OR item_id IN (SELECT item_id FROM items WHERE namespace_id = ?1 AND name LIKE ?4 ESCAPE '\\'))";
        let page_query = format!(
            "SELECT {OPERATION_COLUMNS} FROM operations WHERE namespace_id = ?1 \
             AND state IN ('PENDING','WAITING_RETRY','WAITING_NETWORK','WAITING_AUTHENTICATION','FAILED_PERMANENT') \
             AND (?2 IS NULL OR state = ?2) \
             AND (?3 IS NULL OR operation_type = ?3) \
             AND (?4 IS NULL OR item_id IN (SELECT item_id FROM items WHERE namespace_id = ?1 AND name LIKE ?4 ESCAPE '\\')) \
             ORDER BY (CASE WHEN state = 'FAILED_PERMANENT' THEN 0 ELSE 1 END), priority ASC, created_at ASC \
             LIMIT ?5 OFFSET ?6"
        );

        let (total, total_failed, operations) = self
            .store
            .read(move |conn| {
                let total: i64 = conn.query_row(count_query, params![namespace_id_s, state_s, operation_type_s, search_pattern], |row| row.get(0))?;
                let total_failed: i64 =
                    conn.query_row(failed_count_query, params![namespace_id_s, state_s, operation_type_s, search_pattern], |row| row.get(0))?;
                let mut stmt = conn.prepare(&page_query)?;
                let rows = stmt.query_map(params![namespace_id_s, state_s, operation_type_s, search_pattern, limit, offset], row_to_operation)?;
                let operations = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((total, total_failed, operations))
            })
            .await?;

        Ok(OperationsPage {
            operations,
            total: total.max(0) as u64,
            total_failed: total_failed.max(0) as u64,
        })
    }

    /// Como `pending_operations`, mas só as que já venceram seu backoff
    /// (`next_attempt_at`) — é esta a lista que o dispatcher deve consumir;
    /// `pending_operations` continua expondo tudo (inclusive o que ainda
    /// aguarda retry) para diagnóstico/`/v1/metrics`.
    pub(crate) async fn due_operations(&self) -> Result<Vec<QueuedOperation>, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let now = now_unix();
        let query = format!(
            "SELECT {OPERATION_COLUMNS} FROM operations WHERE namespace_id = ?1 AND state IN ('PENDING','WAITING_RETRY','WAITING_NETWORK','WAITING_AUTHENTICATION') AND (next_attempt_at IS NULL OR next_attempt_at <= ?2) ORDER BY priority ASC, created_at ASC"
        );
        let rows = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(&query)?;
                let rows = stmt.query_map(params![namespace_id_s, now], row_to_operation)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;
        Ok(rows)
    }

    pub async fn mark_operation_blocked_by_conflict(&self, operation_id: OperationId, message: &str) -> Result<(), SyncError> {
        self.update_operation_state(operation_id, OperationState::BlockedByConflict, Some(message.to_string())).await
    }

    /// Remove outras operações `UploadFile` ainda `Pending` deste item que
    /// não sejam `keep_idempotency_key` — a versão nova sempre supera a
    /// antiga (SPEC §13.4 "upload obsoleto por nova versão local →
    /// cancelar versão anterior"). Nunca toca uma que já esteja `Running`:
    /// uma geração já em voo termina seu curso: o pior caso é fazer o
    /// dispatcher reenviar o conteúdo mais novo em seguida, nunca perder a
    /// escrita mais recente.
    pub(crate) async fn supersede_pending_uploads(&self, item_id: ItemId, keep_idempotency_key: &str) -> Result<(), SyncError> {
        let item_id_s = item_id.to_string();
        let keep = keep_idempotency_key.to_string();
        self.store
            .write(move |tx| {
                tx.execute(
                    "DELETE FROM operations WHERE item_id = ?1 AND operation_type = 'UPLOAD_FILE' AND state = 'PENDING' AND idempotency_key <> ?2",
                    params![item_id_s, keep],
                )
            })
            .await?;
        Ok(())
    }
}
