//! Dispatcher do journal — consome `operations` pendentes e as executa
//! contra o provedor real, sempre através do Governor (T3-06/§13, ADR-007).

use super::SyncCore;
use crate::error::SyncError;
use crate::model::QueuedOperation;
use crate::queries::now_unix;
use nexofs_api_governor::OperationClass;
use nexofs_domain::states::OperationType;
use nexofs_domain::{ItemId, RemoteItemId};
use nexofs_provider_api::{
    CreateDirectoryRequest, DeleteItemRequest, MoveItemRequest, ProviderErrorKind, RemoteItem, UploadRequest,
};
use rusqlite::params;

/// Backoff fixo para retries transitórios sem `Retry-After` explícito do
/// provedor — hardening de backoff exponencial fica para quando houver
/// volume real de falhas para calibrar contra (Fase 6).
const DEFAULT_RETRY_DELAY_SECS: i64 = 30;
/// Backoff curto para "pasta pai ainda não sincronizada" — não é uma falha,
/// só uma questão de ordem de despacho que se resolve sozinha em segundos.
const DEPENDENCY_RETRY_DELAY_SECS: i64 = 15;

impl SyncCore {
    /// Exclusão remota síncrona e imediata — fora do journal, para os poucos
    /// lugares que precisam do resultado JÁ, dentro da própria chamada
    /// (T4-08's "remover remoto e manter local", T4-12's `KeepLocal` de
    /// `LocalDeletedRemoteModified`). `NotFound` conta como sucesso (mesmo
    /// objetivo já alcançado por outro caminho).
    pub(crate) async fn delete_remote_item_now(&self, remote_item_id: String, base_remote_version: Option<String>) -> Result<(), SyncError> {
        let account = self.account_ctx.read().await.clone();
        let namespace_remote_id = self.ctx.namespace_remote_id.clone();
        let provider = self.provider.clone();
        let priority = OperationClass::RemoteMutation.default_priority();
        let remote_item_id_for_call = RemoteItemId::from(remote_item_id);
        let result = self
            .execute_governed(self.rate_scope(OperationClass::RemoteMutation), priority, move || async move {
                provider
                    .delete_item(DeleteItemRequest {
                        account,
                        namespace_remote_id,
                        remote_item_id: remote_item_id_for_call,
                        base_remote_version,
                    })
                    .await
            })
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(err) if err.kind == ProviderErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Metadados atuais de um item já sincronizado, direto do provedor —
    /// usado por T4-12's `KeepLocal` para descobrir a versão remota REAL
    /// antes de sobrescrevê-la de propósito (o `If-Match` precisa bater com
    /// o que existe agora, não com a versão base obsoleta que causou o
    /// conflito). `None` quando o item já não existe mais no remoto.
    pub(crate) async fn fetch_current_remote_item(&self, remote_item_id: String) -> Result<Option<RemoteItem>, SyncError> {
        let account = self.account_ctx.read().await.clone();
        let namespace_remote_id = self.ctx.namespace_remote_id.clone();
        let provider = self.provider.clone();
        let priority = OperationClass::InteractiveMetadata.default_priority();
        let remote_item_id_for_call = RemoteItemId::from(remote_item_id);
        let result = self
            .execute_governed(self.rate_scope(OperationClass::InteractiveMetadata), priority, move || async move {
                provider
                    .get_item(nexofs_provider_api::GetItemRequest {
                        account,
                        namespace_remote_id,
                        remote_item_id: remote_item_id_for_call,
                    })
                    .await
            })
            .await?;
        Ok(result)
    }

    /// Consome uma rodada de operações vencidas deste namespace, na ordem
    /// de prioridade do journal. Chamado periodicamente pelo daemon
    /// (`run_background_maintenance`); seguro de chamar concorrentemente
    /// consigo mesmo (cada operação processada é isolada por
    /// `operation_id`) e de não encontrar nada a fazer.
    pub async fn dispatch_pending_operations(&self) -> Result<(), SyncError> {
        for op in self.due_operations().await? {
            let operation_id = op.operation_id;
            if let Err(err) = self.dispatch_one(op).await {
                tracing::warn!(?err, %operation_id, "operação do journal falhou de forma inesperada (bug, não erro do provedor)");
            }
        }
        Ok(())
    }

    async fn dispatch_one(&self, op: QueuedOperation) -> Result<(), SyncError> {
        match op.operation_type {
            OperationType::UploadFile => self.dispatch_upload(op).await,
            OperationType::CreateDirectory => self.dispatch_create_directory(op).await,
            OperationType::RenameItem | OperationType::MoveItem => self.dispatch_move(op).await,
            OperationType::DeleteItem => self.dispatch_delete(op).await,
            // RestoreItem/HydrateItem/PinTree/RefreshChanges/ReconcileNamespace
            // não passam pelo journal de escrita nesta fase — não deveriam
            // aparecer aqui, mas se aparecerem não travam o dispatcher.
            _ => Ok(()),
        }
    }

    /// Resolve o `remote_item_id` do pai de `item_id`, quando já existe no
    /// remoto. `Ok(None)` para "está na raiz" (pai sintético, sem
    /// `remote_item_id`); `Err` sinaliza "ainda não dá para despachar" —
    /// quem chama deve reagendar a operação em vez de tratar como falha.
    async fn resolve_remote_parent(&self, item_id: ItemId) -> Result<Result<Option<RemoteItemId>, ()>, SyncError> {
        let Some(item) = self.get_item(item_id).await? else {
            return Ok(Err(()));
        };
        let Some(parent_id) = item.parent_item_id else {
            return Ok(Ok(None));
        };
        let Some(parent) = self.get_item(parent_id).await? else {
            return Ok(Err(()));
        };
        if parent.item_id == self.bootstrap_root().await? {
            return Ok(Ok(None));
        }
        match parent.remote_item_id {
            Some(id) => Ok(Ok(Some(RemoteItemId::from(id)))),
            None => Ok(Err(())),
        }
    }

    async fn dispatch_upload(&self, op: QueuedOperation) -> Result<(), SyncError> {
        let Some(item_id) = op.item_id else {
            return self.mark_operation_cancelled(op.operation_id).await;
        };
        let Some(item) = self.get_item(item_id).await? else {
            return self.mark_operation_cancelled(op.operation_id).await;
        };
        // T4-09: pasta-mãe em tempestade — o conteúdo já está seguro
        // localmente, só a chamada ao provedor espera por
        // `resume_from_storm_pause`. Continua `Pending`, sem novo backoff
        // (a checagem é local e barata; tentar de novo no próximo tick de
        // 5s não custa uma chamada de rede).
        if item.parent_item_id.is_some_and(|parent| self.is_storm_paused(parent)) {
            return Ok(());
        }

        let parent_remote_item_id = match self.resolve_remote_parent(item_id).await? {
            Ok(parent) => parent,
            Err(()) => {
                self.mark_operation_waiting_retry(op.operation_id, now_unix() + DEPENDENCY_RETRY_DELAY_SECS, "pasta pai ainda não sincronizada")
                    .await?;
                return Ok(());
            }
        };

        self.mark_operation_running(op.operation_id).await?;
        let cache_object_id = item_id.to_string();
        let base_remote_version = op.base_remote_version.clone();

        // Congela o conteúdo antes de ler — uma escrita concorrente do FUSE
        // no arquivo dirty "ao vivo" não pode corromper bytes já em
        // trânsito para o provedor.
        let snapshot_path = match self.cache.snapshot_dirty_for_upload(&cache_object_id).await {
            Ok(path) => path,
            // Bug real: sem este desvio, um item cujo arquivo dirty já foi
            // consumido (upload anterior bem-sucedido, ou um conflito de
            // MOVE_ITEM/RENAME_ITEM resolvido como "manter local" que
            // reenfileirou um upload por engano) ficava tentando de novo
            // para sempre — o arquivo nunca ia voltar a existir sozinho.
            Err(nexofs_content_cache::CacheError::Io(io_err)) if io_err.kind() == std::io::ErrorKind::NotFound => {
                return self.mark_operation_cancelled(op.operation_id).await;
            }
            Err(err) => {
                self.mark_operation_waiting_retry(op.operation_id, now_unix() + DEFAULT_RETRY_DELAY_SECS, &err.to_string()).await?;
                return Ok(());
            }
        };

        let upload_outcome = self.execute_upload(&item, parent_remote_item_id, base_remote_version, &snapshot_path).await;
        let _ = self.cache.remove_upload_snapshot(&cache_object_id);

        match upload_outcome {
            // Bug real: `apply_uploaded_item` grava o resultado no índice
            // local depois que o provedor já aceitou o upload — se essa
            // gravação falhar (ex.: o provedor devolveu um
            // `remote_item_id` que colide com outro item já indexado), o
            // `?` antigo propagava o erro sem nunca tirar a operação de
            // `Running`. Como `due_operations` nunca redespacha uma
            // operação `Running`, ela ficava presa para sempre — nem
            // repetia, nem aparecia como falha em lugar nenhum da UI.
            Ok(uploaded) => match self.apply_uploaded_item(item_id, uploaded).await {
                Ok(()) => self.mark_operation_completed(op.operation_id).await,
                Err(err) => self.mark_operation_failed_permanent(op.operation_id, &err.to_string()).await,
            },
            Err(err) => self.handle_provider_error(op.operation_id, op.operation_type, item_id, err).await,
        }
    }

    async fn execute_upload(
        &self,
        item: &crate::model::IndexedItem,
        parent_remote_item_id: Option<RemoteItemId>,
        base_remote_version: Option<String>,
        snapshot_path: &std::path::Path,
    ) -> nexofs_provider_api::ProviderResult<RemoteItem> {
        let file = tokio::fs::File::open(snapshot_path)
            .await
            .map_err(|e| nexofs_provider_api::ProviderError::new(ProviderErrorKind::Network, e.to_string()))?;
        let size_bytes = file
            .metadata()
            .await
            .map_err(|e| nexofs_provider_api::ProviderError::new(ProviderErrorKind::Network, e.to_string()))?
            .len();

        let account = self.account_ctx.read().await.clone();
        let provider = self.provider.clone();
        let namespace_remote_id = self.ctx.namespace_remote_id.clone();
        let name = item.name.clone();
        let priority = OperationClass::Upload.default_priority();

        let result = self
            .execute_governed(self.rate_scope(OperationClass::Upload), priority, move || async move {
                provider
                    .upload(UploadRequest {
                        account,
                        namespace_remote_id,
                        parent_remote_item_id,
                        name,
                        size_bytes,
                        base_remote_version,
                        content: Box::pin(file),
                        resumable_session_token: None,
                    })
                    .await
            })
            .await?;
        Ok(result.item)
    }

    async fn apply_uploaded_item(&self, item_id: ItemId, remote_item: RemoteItem) -> Result<(), SyncError> {
        let cache_object_id = item_id.to_string();
        self.cache.promote_dirty_to_clean(&cache_object_id).await?;

        let item_id_s = item_id.to_string();
        let remote_id_s = remote_item.remote_item_id.0.clone();
        let version = remote_item.remote_version.clone();
        let content_version = remote_item.remote_content_version.clone();
        let size = remote_item.size_bytes as i64;
        let modified_at = remote_item.remote_modified_at_unix;
        let now = now_unix();

        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE items SET remote_item_id = ?1, remote_version = ?2, remote_content_version = ?3, size_bytes = ?4, remote_modified_at = ?5, remote_state = 'PRESENT', updated_at = ?6 WHERE item_id = ?7",
                    params![remote_id_s, version, content_version, size, modified_at, now, item_id_s],
                )?;
                tx.execute(
                    "UPDATE local_states SET sync_state = 'CLEAN', base_remote_version = ?1, dirty_since = NULL, updated_at = ?2 WHERE item_id = ?3",
                    params![version, now, item_id_s],
                )
            })
            .await?;
        Ok(())
    }

    async fn dispatch_create_directory(&self, op: QueuedOperation) -> Result<(), SyncError> {
        let Some(item_id) = op.item_id else {
            return self.mark_operation_cancelled(op.operation_id).await;
        };
        let Some(item) = self.get_item(item_id).await? else {
            return self.mark_operation_cancelled(op.operation_id).await;
        };
        if item.remote_item_id.is_some() {
            // Já tem contrapartida remota (ex.: reconciliação encontrou uma
            // pasta homônima antes deste dispatch rodar) — nada a fazer.
            return self.mark_operation_completed(op.operation_id).await;
        }
        // T4-09: mesma pausa por tempestade de `dispatch_upload`.
        if item.parent_item_id.is_some_and(|parent| self.is_storm_paused(parent)) {
            return Ok(());
        }

        let parent_remote_item_id = match self.resolve_remote_parent(item_id).await? {
            Ok(parent) => parent,
            Err(()) => {
                self.mark_operation_waiting_retry(op.operation_id, now_unix() + DEPENDENCY_RETRY_DELAY_SECS, "pasta pai ainda não sincronizada")
                    .await?;
                return Ok(());
            }
        };

        self.mark_operation_running(op.operation_id).await?;
        let account = self.account_ctx.read().await.clone();
        let namespace_remote_id = self.ctx.namespace_remote_id.clone();
        let provider = self.provider.clone();
        let name = item.name.clone();
        let priority = OperationClass::RemoteMutation.default_priority();

        let result = self
            .execute_governed(self.rate_scope(OperationClass::RemoteMutation), priority, move || async move {
                provider.create_directory(CreateDirectoryRequest { account, namespace_remote_id, parent_remote_item_id, name }).await
            })
            .await;

        match result {
            // Mesmo risco de `dispatch_upload`: gravar a identidade remota
            // pode falhar (ex.: colisão de `remote_item_id` já indexado)
            // depois que o provedor já aceitou a chamada — sem tratar isso
            // aqui, a operação ficava presa em `Running` para sempre.
            Ok(remote_item) => match self.apply_remote_identity(item_id, &remote_item).await {
                Ok(()) => self.mark_operation_completed(op.operation_id).await,
                Err(err) => self.mark_operation_failed_permanent(op.operation_id, &err.to_string()).await,
            },
            Err(err) => self.handle_provider_error(op.operation_id, op.operation_type, item_id, err).await,
        }
    }

    async fn dispatch_move(&self, op: QueuedOperation) -> Result<(), SyncError> {
        let Some(item_id) = op.item_id else {
            return self.mark_operation_cancelled(op.operation_id).await;
        };
        let Some(item) = self.get_item(item_id).await? else {
            return self.mark_operation_cancelled(op.operation_id).await;
        };
        let Some(remote_item_id) = item.remote_item_id.clone() else {
            return self.mark_operation_cancelled(op.operation_id).await;
        };

        // O índice local já reflete o pai/nome-alvo atuais — `rename_local_item`
        // aplica a mudança de imediato, então reler o item agora já dá a
        // intenção final, mesmo após múltiplos renames em sequência (SPEC
        // §13.4 "múltiplos renames → nome final" resolvido na origem).
        let new_parent_remote_item_id = match self.resolve_remote_parent(item_id).await? {
            Ok(parent) => parent,
            Err(()) => {
                self.mark_operation_waiting_retry(op.operation_id, now_unix() + DEPENDENCY_RETRY_DELAY_SECS, "pasta pai de destino ainda não sincronizada")
                    .await?;
                return Ok(());
            }
        };

        self.mark_operation_running(op.operation_id).await?;
        let account = self.account_ctx.read().await.clone();
        let namespace_remote_id = self.ctx.namespace_remote_id.clone();
        let provider = self.provider.clone();
        let new_name = Some(item.name.clone());
        let base_remote_version = op.base_remote_version.clone();
        let priority = OperationClass::RemoteMutation.default_priority();
        let remote_item_id_for_call = RemoteItemId::from(remote_item_id);

        let result = self
            .execute_governed(self.rate_scope(OperationClass::RemoteMutation), priority, move || async move {
                provider
                    .move_item(MoveItemRequest {
                        account,
                        namespace_remote_id,
                        remote_item_id: remote_item_id_for_call,
                        new_parent_remote_item_id,
                        new_name,
                        base_remote_version,
                    })
                    .await
            })
            .await;

        match result {
            // Mesmo risco de `dispatch_upload`: gravar a identidade remota
            // pode falhar (ex.: colisão de `remote_item_id` já indexado)
            // depois que o provedor já aceitou a chamada — sem tratar isso
            // aqui, a operação ficava presa em `Running` para sempre.
            Ok(remote_item) => match self.apply_remote_identity(item_id, &remote_item).await {
                Ok(()) => self.mark_operation_completed(op.operation_id).await,
                Err(err) => self.mark_operation_failed_permanent(op.operation_id, &err.to_string()).await,
            },
            Err(err) => self.handle_provider_error(op.operation_id, op.operation_type, item_id, err).await,
        }
    }

    async fn dispatch_delete(&self, op: QueuedOperation) -> Result<(), SyncError> {
        let Some(item_id) = op.item_id else {
            return self.mark_operation_cancelled(op.operation_id).await;
        };
        let Some(item) = self.get_item(item_id).await? else {
            return self.mark_operation_completed(op.operation_id).await;
        };
        let Some(remote_item_id) = item.remote_item_id.clone() else {
            return self.mark_operation_cancelled(op.operation_id).await;
        };

        self.mark_operation_running(op.operation_id).await?;
        let account = self.account_ctx.read().await.clone();
        let namespace_remote_id = self.ctx.namespace_remote_id.clone();
        let provider = self.provider.clone();
        let base_remote_version = op.base_remote_version.clone();
        let priority = OperationClass::RemoteMutation.default_priority();
        let remote_item_id_for_call = RemoteItemId::from(remote_item_id);

        let result = self
            .execute_governed(self.rate_scope(OperationClass::RemoteMutation), priority, move || async move {
                provider.delete_item(DeleteItemRequest { account, namespace_remote_id, remote_item_id: remote_item_id_for_call, base_remote_version }).await
            })
            .await;

        // `hard_delete_item` já remove esta própria linha de `operations`
        // (precisa, para não violar a FK `operations.item_id -> items`) —
        // não há um `operation_id` sobrevivente para marcar `Completed`
        // depois, em nenhum dos dois casos de sucesso abaixo.
        match result {
            Ok(()) => self.hard_delete_item(item_id).await,
            // Já não existe remotamente (ex.: apagado por outro cliente
            // primeiro) — o objetivo desta operação já foi alcançado.
            Err(err) if err.kind == ProviderErrorKind::NotFound => self.hard_delete_item(item_id).await,
            Err(err) => self.handle_provider_error(op.operation_id, op.operation_type, item_id, err).await,
        }
    }

    /// Só atualiza a identidade/versão remota — usado por `CreateDirectory`
    /// e `MoveItem`/`RenameItem`, que (ao contrário de `UploadFile`) nunca
    /// mudam conteúdo/tamanho.
    async fn apply_remote_identity(&self, item_id: ItemId, remote_item: &RemoteItem) -> Result<(), SyncError> {
        let item_id_s = item_id.to_string();
        let remote_id_s = remote_item.remote_item_id.0.clone();
        let version = remote_item.remote_version.clone();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE items SET remote_item_id = ?1, remote_version = ?2, updated_at = ?3 WHERE item_id = ?4",
                    params![remote_id_s, version.clone(), now, item_id_s],
                )?;
                // Bug real: sem isto, `local_states.base_remote_version`
                // (a versão que `rename_local_item`/`delete_local_item`
                // usam como base ao enfileirar a PRÓXIMA operação) ficava
                // travada na versão de quando o conteúdo foi enviado pela
                // última vez — nunca avançava com um move/rename. Um
                // segundo move/rename do mesmo item enviava essa versão
                // velha como base e batia num `VersionConflict` fantasma
                // contra a versão que o PRÓPRIO move anterior tinha acabado
                // de produzir, sem nenhum conflito real ter acontecido.
                tx.execute(
                    "UPDATE local_states SET base_remote_version = ?1, updated_at = ?2 WHERE item_id = ?3",
                    params![version, now, item_id_s],
                )
            })
            .await?;
        Ok(())
    }

    /// Classifica o erro do provedor e move a operação para o estado certo
    /// — nunca perde o conteúdo/intenção local (SPEC §16.3): um erro que
    /// `nexofs-conflicts` reconhece como conflito (T3-08, SPEC §18.2) vira
    /// um registro estruturado em `conflicts` além de `BlockedByConflict`
    /// (resolução completa fica para T4-12); falta de rede (`Network`/
    /// `Timeout`) vira `WaitingNetwork`, liberado assim que
    /// `execute_governed` detectar reconexão (T3-09/FR-OFF-005), em vez do
    /// backoff cronometrado genérico; `AuthenticationRequired` tenta renovar
    /// o access token na hora (T7-03) antes de decidir entre retry e falha
    /// permanente; outro erro transitório vira retry respeitando
    /// `Retry-After` quando o provedor o informou; qualquer outro erro é
    /// permanente e exige intervenção manual (visível em `/v1/metrics`).
    async fn handle_provider_error(
        &self,
        operation_id: nexofs_domain::OperationId,
        operation_type: OperationType,
        item_id: ItemId,
        err: nexofs_provider_api::ProviderError,
    ) -> Result<(), SyncError> {
        if let Some(conflict_type) = nexofs_conflicts::classify_provider_error(operation_type, &err.kind) {
            self.record_conflict(item_id, conflict_type, &err.message).await?;
            return self.mark_operation_blocked_by_conflict(operation_id, &err.message).await;
        }
        match err.kind {
            ProviderErrorKind::Network | ProviderErrorKind::Timeout => {
                self.mark_operation_waiting_network(operation_id, &err.message).await
            }
            ProviderErrorKind::AuthenticationRequired => match self.try_refresh_access_token().await {
                Ok(()) => {
                    self.mark_operation_waiting_retry(operation_id, now_unix() + DEFAULT_RETRY_DELAY_SECS, &err.message)
                        .await
                }
                Err(refresh_err) => {
                    tracing::error!(
                        namespace_id = %self.ctx.namespace_id,
                        %refresh_err,
                        "refresh token inválido ou revogado — conta precisa de reautenticação (NEXOFS_ADD_ACCOUNT=1)"
                    );
                    self.mark_operation_failed_permanent(
                        operation_id,
                        &format!("sessão expirada e o refresh token também falhou: {refresh_err}"),
                    )
                    .await
                }
            },
            _ if err.is_transient() => {
                let delay = err.retry_after().map(|d| d.as_secs() as i64).unwrap_or(DEFAULT_RETRY_DELAY_SECS);
                self.mark_operation_waiting_retry(operation_id, now_unix() + delay, &err.message).await
            }
            _ => self.mark_operation_failed_permanent(operation_id, &err.message).await,
        }
    }

    /// Renova `account_ctx.access_token` via `refresh_token` guardado —
    /// chamado sob demanda quando uma chamada real ao provedor volta com
    /// `AuthenticationRequired`, em vez de só na inicialização do daemon
    /// (era isso que faltava: antes disso, o access token nunca era
    /// renovado depois do mount inicial, então expirava silenciosamente
    /// ~1h depois e todo o journal ficava preso). `token_refresh_lock`
    /// serializa tentativas concorrentes desta mesma conta.
    pub(crate) async fn try_refresh_access_token(&self) -> nexofs_provider_api::ProviderResult<()> {
        let _guard = self.token_refresh_lock.lock().await;
        let Some(current_refresh_token) = self.refresh_token.read().await.clone() else {
            return Err(nexofs_provider_api::ProviderError::new(
                ProviderErrorKind::AuthenticationRequired,
                "nenhum refresh token disponível para renovar a sessão — reautentique com NEXOFS_ADD_ACCOUNT=1",
            ));
        };
        let refreshed = self.provider.refresh_via_refresh_token(&current_refresh_token).await?;

        {
            let mut ctx = self.account_ctx.write().await;
            ctx.access_token = refreshed.access_token;
            ctx.tenant_id = refreshed.tenant_id;
        }
        *self.refresh_token.write().await = Some(refreshed.refresh_token);

        tracing::info!(namespace_id = %self.ctx.namespace_id, "access token renovado em tempo de execução após expirar");
        Ok(())
    }
}
