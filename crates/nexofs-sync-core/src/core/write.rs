//! Escrita local: copy-on-write (SPEC §16.1), criação/exclusão/renomeio
//! locais e o gatilho de estabilização que enfileira o upload no journal.

use super::SyncCore;
use crate::error::SyncError;
use crate::model::item_kind_to_sql;
use crate::queries::{now_unix, parse_item_id};
use nexofs_api_governor::OperationClass;
use nexofs_domain::states::{OperationType, SyncDisposition};
use nexofs_domain::ItemId;
use nexofs_provider_api::ItemKind;
use rusqlite::{params, OptionalExtension};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// SPEC §16.2: "5 segundos sem nova escrita" — um dos quatro gatilhos de
/// estabilização.
const WRITE_IDLE_DEBOUNCE: Duration = Duration::from_secs(5);

struct LocalState {
    sync_state: String,
    local_version: i64,
    base_remote_version: Option<String>,
}

impl SyncCore {
    async fn local_state_of(&self, item_id: ItemId) -> Result<Option<LocalState>, SyncError> {
        let item_id_s = item_id.to_string();
        let row = self
            .store
            .read(move |conn| {
                conn.query_row(
                    "SELECT sync_state, local_version, base_remote_version FROM local_states WHERE item_id = ?1",
                    [item_id_s],
                    |row| {
                        Ok(LocalState {
                            sync_state: row.get(0)?,
                            local_version: row.get(1)?,
                            base_remote_version: row.get(2)?,
                        })
                    },
                )
                .optional()
            })
            .await?;
        Ok(row)
    }

    /// SPEC §16.1: materializa a cópia dirty na primeira escrita sobre um
    /// item `Clean` (ou cria um arquivo vazio, para um item novo) e responde
    /// sem tocar rede. Idempotente dentro da mesma geração dirty — chamadas
    /// repetidas (uma por `write()` do FUSE) reaproveitam o mesmo arquivo.
    pub async fn begin_write(&self, item_id: ItemId) -> Result<PathBuf, SyncError> {
        let item = self.get_item(item_id).await?.ok_or(SyncError::NotFound)?;
        if item.kind == ItemKind::Directory {
            return Err(SyncError::InvalidOperation("não é possível escrever em um diretório"));
        }
        if item.source_layer == "LOCAL_ONLY" {
            return self.begin_local_only_write(item_id).await;
        }

        let cache_object_id = item_id.to_string();
        if self.cache.has_dirty(&cache_object_id) {
            return Ok(self.cache.dirty_path(&cache_object_id));
        }

        let base_clean = self.cache.is_hydrated(&cache_object_id).then(|| self.cache.clean_path(&cache_object_id));
        let dirty_path = self.cache.begin_dirty_write(&cache_object_id, base_clean.as_deref()).await?;

        let item_id_s = item_id.to_string();
        let cache_object_id_s = cache_object_id.clone();
        let base_remote_version = item.remote_version.clone();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO local_states (item_id, hydration_state, pin_state, sync_state, local_version, base_remote_version, cache_object_id, local_size_bytes, dirty_since, last_access_at, open_handle_count, updated_at) \
                     VALUES (?1, 'HYDRATED', 'AVAILABLE_LOCALLY', 'DIRTY', 1, ?2, ?3, 0, ?4, ?4, 0, ?4) \
                     ON CONFLICT(item_id) DO UPDATE SET sync_state = 'DIRTY', local_version = local_states.local_version + 1, cache_object_id = excluded.cache_object_id, dirty_since = COALESCE(local_states.dirty_since, excluded.dirty_since), updated_at = excluded.updated_at",
                    params![item_id_s, base_remote_version, cache_object_id_s, now],
                )
            })
            .await?;

        Ok(dirty_path)
    }

    /// `setattr` com `size` definido — trunca (ou estende com zeros) o
    /// conteúdo dirty, criando-o primeiro se ainda não existir (mesmo
    /// comportamento de `begin_write`, SPEC §16.1 "`setattr/truncate`" na
    /// tabela de capacidades FUSE).
    pub async fn truncate_local(&self, item_id: ItemId, size: u64) -> Result<(), SyncError> {
        let path = self.begin_write(item_id).await?;
        tokio::task::spawn_blocking(move || std::fs::OpenOptions::new().write(true).open(&path)?.set_len(size))
            .await
            .expect("spawn_blocking não deve entrar em pânico")?;
        self.update_local_size(item_id, size).await
    }

    /// Atualiza o tamanho local reportado por `getattr` enquanto o arquivo
    /// segue aberto para escrita — chamado após `truncate_local` e por
    /// `write()` do FUSE a cada gravação.
    pub async fn update_local_size(&self, item_id: ItemId, size: u64) -> Result<(), SyncError> {
        let item_id_s = item_id.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE local_states SET local_size_bytes = ?1, updated_at = ?2 WHERE item_id = ?3",
                    params![size as i64, now, item_id_s],
                )
            })
            .await?;
        Ok(())
    }

    /// T3-04/SPEC §16.2, terceiro gatilho de estabilização ("5 segundos sem
    /// nova escrita"). `nexofs-fuse::write()` chama isto a cada gravação —
    /// cancela qualquer agendamento anterior deste item e agenda um novo,
    /// de forma que só a ÚLTIMA escrita dentro da janela realmente dispare
    /// `stabilize_upload`, sem nunca disparar cedo demais enquanto a
    /// aplicação ainda está escrevendo (ex.: um editor gravando em blocos).
    /// Precisa de `Arc<Self>` (não só `&self`) porque a tarefa agendada
    /// sobrevive além desta chamada.
    pub fn schedule_write_idle_stabilization(self: &Arc<Self>, item_id: ItemId) {
        let mut tasks = self.write_idle_debounce.lock().expect("lock síncrono, nunca mantido durante um await");
        if let Some(previous) = tasks.remove(&item_id) {
            previous.abort();
        }

        let core = self.clone();
        let handle = self.runtime_handle.spawn(async move {
            tokio::time::sleep(WRITE_IDLE_DEBOUNCE).await;
            if let Err(err) = core.stabilize_upload(item_id).await {
                tracing::warn!(?err, %item_id, "falha ao estabilizar upload após debounce de escrita ociosa");
            }
            core.write_idle_debounce.lock().expect("lock síncrono").remove(&item_id);
        });
        tasks.insert(item_id, handle);
    }

    /// `create`/`mkdir`. Um arquivo novo (mesmo vazio) já é conteúdo local
    /// sem contrapartida remota — fica `Dirty` desde a criação (cobre
    /// `touch`/`O_CREAT` puro, sem exigir um `write()` real). Um diretório
    /// não tem conteúdo a estabilizar: sua operação de criação remota é
    /// enfileirada imediatamente.
    ///
    /// T4-01/T4-05 (SPEC §17): antes de criar, avalia o caminho contra as
    /// regras de exclusão do namespace. Um resultado `LocalOnly` nunca gera
    /// operação de journal — o item vive só no overlay (`source_layer =
    /// 'LOCAL_ONLY'`), pelo tempo que a regra estiver ativa (SPEC §11.4).
    pub async fn create_local_item(&self, parent_item_id: ItemId, name: &str, kind: ItemKind) -> Result<ItemId, SyncError> {
        self.ensure_children_loaded(parent_item_id).await?;

        let candidate_path = self.item_relative_path(parent_item_id).await?.join(name);
        let engine = self.ignore_engine().await?;
        let is_local_only = engine.evaluate(&candidate_path, kind == ItemKind::Directory).disposition == SyncDisposition::LocalOnly;
        let source_layer = if is_local_only { "LOCAL_ONLY" } else { "LOCAL" };

        let namespace_id_s = self.ctx.namespace_id.to_string();
        let parent_id_s = parent_item_id.to_string();
        let name_owned = name.to_string();
        let normalized = name.to_lowercase();
        let kind_s = item_kind_to_sql(kind);
        let now = now_unix();

        let created = self
            .store
            .write(move |tx| {
                let collision: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM items WHERE namespace_id = ?1 AND parent_item_id = ?2 AND normalized_name = ?3 AND remote_state <> 'DELETED'",
                        params![namespace_id_s, parent_id_s, normalized],
                        |row| row.get(0),
                    )
                    .optional()?;
                if collision.is_some() {
                    return Ok(None);
                }

                let new_id = ItemId::new();
                let new_id_s = new_id.to_string();
                tx.execute(
                    "INSERT INTO items (item_id, namespace_id, remote_item_id, parent_item_id, name, normalized_name, item_type, size_bytes, children_state, remote_state, source_layer, created_at, updated_at) \
                     VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, 0, 'LOADED', 'PRESENT', ?7, ?8, ?8)",
                    params![new_id_s, namespace_id_s, parent_id_s, name_owned, normalized, kind_s, source_layer, now],
                )?;
                Ok(Some(new_id))
            })
            .await?;

        let item_id = created.ok_or(SyncError::AlreadyExists)?;

        // T4-09/SPEC §7.9: registra o evento de criação independentemente
        // do resultado da exclusão — mesmo um `LocalOnly` conta para a taxa
        // (é a PASTA que está gerando volume, o que interessa aqui). A
        // operação abaixo ainda é enfileirada normalmente mesmo em
        // tempestade — quem barra o efeito remoto é o dispatcher
        // (`dispatch_create_directory`/`dispatch_upload` checam
        // `is_storm_paused` antes de chamar o provedor), não a criação: se
        // parasse de enfileirar aqui, a intenção de sincronizar se perderia
        // para sempre em vez de só esperar a retomada.
        if let Some(event) = self.record_creation_and_check_storm(parent_item_id, name, is_local_only).await {
            tracing::warn!(
                folder_item_id = %event.folder_item_id,
                items_in_window = event.items_in_window,
                risk = ?event.risk,
                "criação dentro de pasta em tempestade — operação remota adiada até retomada explícita"
            );
        }

        match (kind, is_local_only) {
            (ItemKind::File, true) => {
                self.begin_local_only_write(item_id).await?;
            }
            (ItemKind::File, false) => {
                self.begin_write(item_id).await?;
            }
            (ItemKind::Directory, true) => {
                // Nada a enfileirar — a pasta inteira nunca terá contrapartida
                // remota enquanto a exclusão estiver ativa (SPEC §11.4).
            }
            (ItemKind::Directory, false) => {
                let priority = OperationClass::RemoteMutation.default_priority();
                let payload = serde_json::json!({ "parent_item_id": parent_item_id.to_string(), "name": name }).to_string();
                self.enqueue_operation(
                    Some(item_id),
                    OperationType::CreateDirectory,
                    format!("create_dir:{}:{}", self.ctx.namespace_id, item_id),
                    priority.0,
                    None,
                    payload,
                )
                .await?;
            }
        }

        Ok(item_id)
    }

    /// Equivalente de `begin_write` para um item `LocalOnly` (T4-05):
    /// materializa no overlay em vez do cache dirty, marca
    /// `local_states.sync_state = 'LOCAL_ONLY'` (nunca vira `Dirty`/`Clean`
    /// — não há o que sincronizar) e nunca toca o journal.
    pub(crate) async fn begin_local_only_write(&self, item_id: ItemId) -> Result<PathBuf, SyncError> {
        let cache_object_id = item_id.to_string();
        let path = self.overlay.create_empty(&cache_object_id).await?;

        let item_id_s = item_id.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO local_states (item_id, hydration_state, pin_state, sync_state, cache_object_id, local_size_bytes, last_access_at, open_handle_count, updated_at) \
                     VALUES (?1, 'HYDRATED', 'AVAILABLE_LOCALLY', 'LOCAL_ONLY', ?2, 0, ?3, 0, ?3) \
                     ON CONFLICT(item_id) DO UPDATE SET cache_object_id = excluded.cache_object_id, updated_at = excluded.updated_at",
                    params![item_id_s, cache_object_id, now],
                )
            })
            .await?;

        Ok(path)
    }

    /// `flush`/`fsync`/`release` do último handle gravável — dois dos quatro
    /// gatilhos de estabilização de SPEC §16.2 (o debounce de 5s ocioso e o
    /// comando manual ficam para hardening futuro do dispatcher). Sem efeito
    /// sobre um item que não está `Dirty` — chamar em qualquer `flush`,
    /// mesmo de um arquivo aberto só para leitura, é seguro.
    pub async fn stabilize_upload(&self, item_id: ItemId) -> Result<(), SyncError> {
        let Some(local) = self.local_state_of(item_id).await? else {
            return Ok(());
        };
        if local.sync_state != "DIRTY" {
            return Ok(());
        }

        let priority = OperationClass::Upload.default_priority();
        let payload = serde_json::json!({ "cache_object_id": item_id.to_string() }).to_string();
        let idempotency_key = format!("upload:{}:{}:{}", self.ctx.namespace_id, item_id, local.local_version);
        self.enqueue_operation(
            Some(item_id),
            OperationType::UploadFile,
            idempotency_key.clone(),
            priority.0,
            local.base_remote_version,
            payload,
        )
        .await?;
        // Qualquer UploadFile mais antigo ainda `Pending` para este item
        // referenciava uma geração de conteúdo já superada — não deve mais
        // ser despachado (SPEC §13.4).
        self.supersede_pending_uploads(item_id, &idempotency_key).await?;
        Ok(())
    }

    /// T3-04/SPEC §16.2, quarto e último gatilho de estabilização ("comando
    /// manual"). Estabiliza todo item ainda `Dirty` deste namespace de uma
    /// vez — usado pelo endpoint local de "sincronizar agora" (Fase 5) para
    /// não obrigar o usuário a esperar o debounce de 5s ou fechar cada
    /// arquivo manualmente. Retorna quantos itens foram estabilizados.
    pub async fn stabilize_all_dirty_items(&self) -> Result<u64, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let dirty_item_ids: Vec<String> = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT items.item_id FROM items JOIN local_states ON local_states.item_id = items.item_id \
                     WHERE items.namespace_id = ?1 AND local_states.sync_state = 'DIRTY'",
                )?;
                let rows = stmt.query_map([namespace_id_s], |row| row.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;

        for item_id_s in &dirty_item_ids {
            self.stabilize_upload(parse_item_id(item_id_s)).await?;
        }
        Ok(dirty_item_ids.len() as u64)
    }

    /// `unlink`/`rmdir`. `expected_kind` reflete a semântica POSIX de cada
    /// chamada: `unlink` sobre um diretório retorna `IsADirectory` (→
    /// `EISDIR`), `rmdir` sobre um arquivo retorna `NotADirectory` (→
    /// `ENOTDIR`) — nenhum dos dois apaga o item errado por engano. Um item
    /// nunca sincronizado (`remote_item_id` nulo) não deixa rastro remoto
    /// para reconciliar — some do índice por completo e qualquer operação
    /// `Pending` associada é cancelada (SPEC §13.4 "create + delete antes do
    /// upload → cancelar ambos"). Um item com contrapartida remota vira
    /// `DELETED_LOCALLY` (tombstone local, distinto do tombstone remoto de
    /// `tombstone_item`) e some das listagens até o `DeleteItem` confirmar
    /// no lado remoto.
    pub async fn delete_local_item(&self, parent_item_id: ItemId, name: &str, expected_kind: ItemKind) -> Result<(), SyncError> {
        let item = self.lookup_child(parent_item_id, name).await?.ok_or(SyncError::NotFound)?;

        if item.kind != expected_kind {
            return Err(if expected_kind == ItemKind::Directory {
                SyncError::NotADirectory
            } else {
                SyncError::IsADirectory
            });
        }

        if item.kind == ItemKind::Directory {
            let children = self.list_children(item.item_id).await?;
            if !children.is_empty() {
                return Err(SyncError::NotEmpty);
            }
        }

        self.cancel_pending_operations_for_item(item.item_id).await?;

        match &item.remote_item_id {
            None => self.hard_delete_item(item.item_id).await?,
            Some(_) => {
                let item_id_s = item.item_id.to_string();
                let now = now_unix();
                self.store
                    .write(move |tx| {
                        tx.execute(
                            "INSERT INTO local_states (item_id, hydration_state, pin_state, sync_state, last_access_at, open_handle_count, updated_at) \
                             VALUES (?1, 'PLACEHOLDER', 'ONLINE_ONLY', 'DELETED_LOCALLY', ?2, 0, ?2) \
                             ON CONFLICT(item_id) DO UPDATE SET sync_state = 'DELETED_LOCALLY', updated_at = excluded.updated_at",
                            params![item_id_s, now],
                        )
                    })
                    .await?;
                let _ = self.cache.remove_dirty(&item.item_id.to_string());

                let priority = OperationClass::RemoteMutation.default_priority();
                let local = self.local_state_of(item.item_id).await?;
                let local_version = local.as_ref().map(|s| s.local_version).unwrap_or(0);
                // Um arquivo que já esteve `Dirty` tem em `local_states` a
                // versão remota que sua última intenção local realmente
                // conhecia; `items.remote_version` pode já ter avançado por
                // conta de um `refresh_changes` incidental entre a edição e
                // esta exclusão, e usá-lo mascararia o próprio conflito que
                // o controle otimista de versão existe para detectar
                // (LocalDeletedRemoteModified, T3-08).
                //
                // Diretórios nunca passam por `begin_write` (sem conteúdo
                // próprio), então não têm essa versão congelada — e não
                // deveriam ser verificados contra `items.remote_version` de
                // qualquer forma: bug real encontrado validando esta fase —
                // o Graph avança o eTag de uma pasta sempre que um filho seu
                // muda (upload/exclusão dentro dela), então o `remote_version`
                // capturado na criação da pasta fica obsoleto assim que
                // qualquer arquivo é criado/apagado dentro dela, fazendo
                // `rmdir`/`mv` de uma pasta com conteúdo recém-modificado
                // falhar com `VersionConflict` mesmo sem nenhuma edição
                // concorrente real da pasta em si — a garantia que já importa
                // para uma pasta (estar vazia) já é verificada acima via
                // `list_children`.
                let base_remote_version = match item.kind {
                    ItemKind::File => local.and_then(|s| s.base_remote_version).or_else(|| item.remote_version.clone()),
                    ItemKind::Directory => None,
                };
                let payload = serde_json::json!({ "remote_item_id": item.remote_item_id }).to_string();
                self.enqueue_operation(
                    Some(item.item_id),
                    OperationType::DeleteItem,
                    format!("delete:{}:{}:{}", self.ctx.namespace_id, item.item_id, local_version),
                    priority.0,
                    base_remote_version,
                    payload,
                )
                .await?;
            }
        }

        Ok(())
    }

    /// `rename` (inclui mover entre diretórios). Aplica a mudança no índice
    /// local de imediato — a aplicação chamadora não espera confirmação
    /// remota — e enfileira `RenameItem`/`MoveItem` para um item já
    /// sincronizado. A chave de idempotência não inclui o alvo: uma segunda
    /// chamada de rename sobre o mesmo item antes do dispatcher consumir a
    /// primeira atualiza o payload em vez de duplicar (SPEC §13.4
    /// "múltiplos renames → nome final").
    pub async fn rename_local_item(
        &self,
        old_parent_item_id: ItemId,
        old_name: &str,
        new_parent_item_id: ItemId,
        new_name: &str,
    ) -> Result<(), SyncError> {
        let item = self.lookup_child(old_parent_item_id, old_name).await?.ok_or(SyncError::NotFound)?;

        self.ensure_children_loaded(new_parent_item_id).await?;
        if let Some(existing) = self.lookup_child(new_parent_item_id, new_name).await? {
            if existing.item_id != item.item_id {
                return Err(SyncError::AlreadyExists);
            }
        }

        let item_id_s = item.item_id.to_string();
        let new_parent_id_s = new_parent_item_id.to_string();
        let new_name_owned = new_name.to_string();
        let normalized = new_name.to_lowercase();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "UPDATE items SET parent_item_id = ?1, name = ?2, normalized_name = ?3, updated_at = ?4 WHERE item_id = ?5",
                    params![new_parent_id_s, new_name_owned, normalized, now, item_id_s],
                )
            })
            .await?;

        if item.remote_item_id.is_some() {
            let moved = new_parent_item_id != old_parent_item_id;
            let op_type = if moved { OperationType::MoveItem } else { OperationType::RenameItem };
            let op_name = if moved { "move" } else { "rename" };
            let priority = OperationClass::RemoteMutation.default_priority();
            let payload = serde_json::json!({
                "target_parent_item_id": new_parent_item_id.to_string(),
                "target_name": new_name,
            })
            .to_string();
            // Mesma ressalva de `delete_local_item`: para arquivos, prefere a
            // versão congelada em `local_states` à `items.remote_version`
            // (que pode ter avançado por delta); para diretórios, nenhuma —
            // o eTag de uma pasta no Graph avança sempre que um filho seu
            // muda, então verificar `base_remote_version` nela faria
            // qualquer `mv`/rename de uma pasta com conteúdo recém-alterado
            // falhar com `VersionConflict` sem nenhum conflito real (mesmo
            // bug encontrado em `delete_local_item`, ver o comentário lá).
            let base_remote_version = match item.kind {
                ItemKind::File => self
                    .local_state_of(item.item_id)
                    .await?
                    .and_then(|s| s.base_remote_version)
                    .or_else(|| item.remote_version.clone()),
                ItemKind::Directory => None,
            };
            self.enqueue_operation(
                Some(item.item_id),
                op_type,
                format!("{op_name}:{}:{}", self.ctx.namespace_id, item.item_id),
                priority.0,
                base_remote_version,
                payload,
            )
            .await?;
        }

        Ok(())
    }

    /// Remove um item por completo do índice — item, estado local e
    /// qualquer linha do journal que o referencie. Usado tanto por um item
    /// nunca sincronizado apagado localmente (nada a reconciliar no remoto)
    /// quanto pelo dispatcher após uma exclusão remota confirmada.
    ///
    /// A ordem importa: `operations.item_id` referencia `items.item_id`
    /// (`FOREIGN KEY`, `foreign_keys=ON` — SPEC §10.2); apagar `items`
    /// primeiro violaria a constraint sempre que existisse qualquer
    /// operação (mesmo já `Completed`/`Cancelled`, ou a própria operação de
    /// exclusão em curso) ainda referenciando este item — bug real
    /// encontrado validando o dispatcher: a operação `DeleteItem` ficava
    /// presa em `Running` para sempre porque a exclusão do `items` falhava
    /// silenciosamente na transação.
    pub(crate) async fn hard_delete_item(&self, item_id: ItemId) -> Result<(), SyncError> {
        let item_id_s = item_id.to_string();
        self.store
            .write(move |tx| {
                // `conflicts.item_id` também referencia `items.item_id`
                // (mesma FK, mesmo `foreign_keys=ON` — bug real encontrado
                // validando T4-12: resolver `KeepLocal` de um
                // `LocalDeletedRemoteModified` chama `hard_delete_item` com
                // o conflito ainda `OPEN`, e sem esta linha a exclusão de
                // `items` violava a constraint, igual ao bug de
                // `operations` já corrigido na Fase 3).
                tx.execute("DELETE FROM conflicts WHERE item_id = ?1", [&item_id_s])?;
                tx.execute("DELETE FROM operations WHERE item_id = ?1", [&item_id_s])?;
                tx.execute("DELETE FROM local_states WHERE item_id = ?1", [&item_id_s])?;
                tx.execute("DELETE FROM items WHERE item_id = ?1", [&item_id_s])
            })
            .await?;
        let cache_object_id = item_id.to_string();
        let _ = self.cache.remove_dirty(&cache_object_id);
        let _ = self.cache.remove(&cache_object_id);
        let _ = self.overlay.remove(&cache_object_id);
        Ok(())
    }
}
