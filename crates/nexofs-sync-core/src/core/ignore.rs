//! Avaliação de exclusão (T4-01 a T4-04, SPEC §17) — carrega as regras do
//! namespace do banco, compila um `nexofs_ignore::IgnoreEngine` (cacheado) e
//! resolve o caminho de um item para avaliação. A decisão real de "isso vira
//! `LocalOnly`" mora aqui; o que fazer com o resultado (não enfileirar
//! journal, usar o overlay em vez do cache) é de quem chama
//! (`core/write.rs`).

use super::SyncCore;
use crate::error::SyncError;
use crate::queries::now_unix;
use nexofs_api_governor::OperationClass;
use nexofs_domain::states::OperationType;
use nexofs_domain::ItemId;
use nexofs_ignore::{IgnoreEngine, Rule, RuleTier};
use nexofs_provider_api::ItemKind;
use rusqlite::params;
use std::path::PathBuf;
use std::sync::Arc;

/// FR-LOC-006: estimativa de custo de uma migração `LocalOnly` →
/// sincronização normal (ou o inverso), calculada ANTES de qualquer
/// execução — para o usuário decidir com informação, não às cegas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MigrationEstimate {
    pub item_count: u64,
    pub total_bytes: u64,
}

fn rule_tier_to_sql(tier: RuleTier) -> &'static str {
    match tier {
        RuleTier::Defaults => "DEFAULT",
        RuleTier::AdminPolicy => "ADMIN_POLICY",
        RuleTier::TechProfile => "TECH_PROFILE",
        RuleTier::UserGlobal => "USER_GLOBAL",
        RuleTier::Account => "ACCOUNT",
        RuleTier::Folder => "FOLDER",
        RuleTier::NexofsIgnoreFile => "NEXOFSIGNORE_FILE",
        RuleTier::UserException => "USER_EXCEPTION",
    }
}

fn rule_tier_from_sql(value: &str) -> Option<RuleTier> {
    Some(match value {
        "DEFAULT" => RuleTier::Defaults,
        "ADMIN_POLICY" => RuleTier::AdminPolicy,
        "TECH_PROFILE" => RuleTier::TechProfile,
        "USER_GLOBAL" => RuleTier::UserGlobal,
        "ACCOUNT" => RuleTier::Account,
        "FOLDER" => RuleTier::Folder,
        "NEXOFSIGNORE_FILE" => RuleTier::NexofsIgnoreFile,
        "USER_EXCEPTION" => RuleTier::UserException,
        _ => return None,
    })
}

impl SyncCore {
    /// Caminho relativo à raiz do namespace (sem o nome da própria raiz
    /// sintética), `/`-separado independente do SO — chave de avaliação de
    /// exclusão (SPEC §17). Público porque `nexofs-local-api` também usa
    /// para exibir caminhos legíveis (log de sincronização, conflitos).
    pub async fn item_relative_path(&self, item_id: ItemId) -> Result<PathBuf, SyncError> {
        let root = self.bootstrap_root().await?;
        let mut segments = Vec::new();
        let mut current = item_id;
        loop {
            if current == root {
                break;
            }
            let item = self.get_item(current).await?.ok_or(SyncError::NotFound)?;
            segments.push(item.name);
            match item.parent_item_id {
                Some(parent) if parent != current => current = parent,
                _ => break,
            }
        }
        segments.reverse();
        Ok(segments.into_iter().collect())
    }

    /// Regras hoje aplicáveis a este namespace, prontas para
    /// `IgnoreEngine::build`. Escopo desta entrega: apenas regras
    /// namespace-wide (`root_item_id IS NULL`) — regras de pasta específica
    /// e arquivos `.nexofsignore` individuais (mesmas camadas `Folder`/
    /// `NexofsIgnoreFile` já suportadas pelo motor) exigem reancorar o
    /// padrão ao caminho do próprio `root_item_id` da regra, o que depende
    /// da varredura de árvore em tempo real que ainda não existe — ficam
    /// para quando essa varredura for ligada, sem mudar o motor em si.
    async fn load_ignore_rules(&self) -> Result<Vec<Rule>, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let rows: Vec<(String, String)> = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT source_type, pattern FROM ignore_rules \
                     WHERE (namespace_id = ?1 OR namespace_id IS NULL) AND root_item_id IS NULL AND enabled = 1 \
                     ORDER BY precedence ASC",
                )?;
                let rows = stmt.query_map([namespace_id_s], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|(source_type, pattern)| rule_tier_from_sql(&source_type).map(|tier| Rule::new(tier, pattern)))
            .collect())
    }

    /// Motor de avaliação compilado, cacheado em memória — reconstruído sob
    /// demanda após `add_ignore_rule`/`remove_ignore_rule`. Um padrão
    /// inválido (ex.: regra corrompida inserida por fora) não pode travar
    /// leitura/escrita — loga e segue com um motor vazio (tudo sincroniza
    /// normalmente) em vez de propagar erro.
    pub(crate) async fn ignore_engine(&self) -> Result<Arc<IgnoreEngine>, SyncError> {
        if let Some(engine) = self.ignore_engine_cache.read().await.as_ref() {
            return Ok(engine.clone());
        }

        let rules = self.load_ignore_rules().await?;
        let engine = Arc::new(match IgnoreEngine::build(&rules) {
            Ok(engine) => engine,
            Err(err) => {
                tracing::warn!(%err, namespace_id = %self.ctx.namespace_id, "regras de exclusão inválidas — sincronizando sem exclusões até serem corrigidas");
                IgnoreEngine::build(&[]).expect("lista vazia sempre compila")
            }
        });

        *self.ignore_engine_cache.write().await = Some(engine.clone());
        Ok(engine)
    }

    async fn invalidate_ignore_engine(&self) {
        *self.ignore_engine_cache.write().await = None;
    }

    /// T4-04: sugestão de perfil por manifesto — adiciona uma regra de
    /// `RuleTier::TechProfile` namespace-wide. Nunca chamado automaticamente:
    /// quem detecta o manifesto (`package.json`, `composer.json`, ...) e
    /// decide oferecer o perfil ao usuário fica de fora deste núcleo
    /// (Fase 5, UI) — SPEC §17.4 "perfis sugeridos NÃO DEVEM ser ativados
    /// silenciosamente sem confirmação". Esta função é o "sim, aplique"
    /// depois dessa confirmação explícita, reaproveitável por qualquer
    /// camada (perfil, conta, exceção do usuário).
    pub async fn add_ignore_rule(&self, tier: RuleTier, pattern: &str) -> Result<(), SyncError> {
        let rule_id = uuid::Uuid::new_v4().to_string();
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let source_type = rule_tier_to_sql(tier);
        let pattern_owned = pattern.to_string();
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO ignore_rules (rule_id, namespace_id, root_item_id, source_type, pattern, negated, directory_only, enabled, precedence, created_at, updated_at) \
                     VALUES (?1, ?2, NULL, ?3, ?4, 0, 0, 1, 0, ?5, ?5)",
                    rusqlite::params![rule_id, namespace_id_s, source_type, pattern_owned, now],
                )
            })
            .await?;
        self.invalidate_ignore_engine().await;
        Ok(())
    }

    /// Lista as regras hoje ativas neste namespace (T4-03: diagnóstico/API
    /// local) — mesmo filtro namespace-wide de `load_ignore_rules`.
    pub async fn list_ignore_rules(&self) -> Result<Vec<Rule>, SyncError> {
        self.load_ignore_rules().await
    }

    /// T5-06: mesma lista de `list_ignore_rules`, mas com o `rule_id` que
    /// `Rule` (tipo puro do motor `nexofs-ignore`, sem noção de banco) não
    /// carrega — é o que a tela de exclusões precisa para oferecer um botão
    /// "remover" por linha.
    pub async fn list_ignore_rules_with_ids(&self) -> Result<Vec<(String, RuleTier, String)>, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let rows: Vec<(String, String, String)> = self
            .store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT rule_id, source_type, pattern FROM ignore_rules \
                     WHERE (namespace_id = ?1 OR namespace_id IS NULL) AND root_item_id IS NULL AND enabled = 1 \
                     ORDER BY precedence ASC",
                )?;
                let rows = stmt.query_map([namespace_id_s], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|(rule_id, source_type, pattern)| rule_tier_from_sql(&source_type).map(|tier| (rule_id, tier, pattern)))
            .collect())
    }

    /// T5-06 ("remover"): apaga uma regra específica pelo `rule_id`, restrita
    /// a este namespace (ou a uma regra global sem namespace — mesmo escopo
    /// que `load_ignore_rules` já enxerga) — nunca deixa apagar a de outra
    /// conta por engano. `Ok(false)` quando o id não corresponde a nada
    /// visível daqui, para o endpoint responder 404 em vez de fingir sucesso.
    pub async fn remove_ignore_rule(&self, rule_id: &str) -> Result<bool, SyncError> {
        let namespace_id_s = self.ctx.namespace_id.to_string();
        let rule_id_owned = rule_id.to_string();
        let affected = self
            .store
            .write(move |tx| {
                tx.execute(
                    "DELETE FROM ignore_rules WHERE rule_id = ?1 AND (namespace_id = ?2 OR namespace_id IS NULL)",
                    params![rule_id_owned, namespace_id_s],
                )
            })
            .await?;
        if affected > 0 {
            self.invalidate_ignore_engine().await;
        }
        Ok(affected > 0)
    }

    /// T4-04: perfis de tecnologia sugeríveis, a partir de manifestos
    /// encontrados na raiz do namespace (`package.json`, `composer.json`,
    /// ...) — só verifica a raiz (onde esses arquivos convencionalmente
    /// vivem), não a árvore inteira. Um perfil cujas regras já foram
    /// aplicadas (por uma sugestão anterior aceita) não é sugerido de novo.
    /// Nunca aplica nada sozinho — SPEC §17.4.
    pub async fn suggest_ignore_profiles(&self) -> Result<Vec<nexofs_ignore::Profile>, SyncError> {
        let root = self.bootstrap_root().await?;
        let children = self.list_children(root).await?;
        let already_active = self.load_ignore_rules().await?;

        let mut suggestions = Vec::new();
        for profile in nexofs_ignore::KNOWN_PROFILES {
            let manifest_present = children.iter().any(|item| item.name == profile.manifest_file);
            if !manifest_present {
                continue;
            }
            let already_applied = profile.patterns.iter().all(|pattern| already_active.iter().any(|rule| rule.pattern == *pattern));
            if !already_applied {
                suggestions.push(*profile);
            }
        }
        Ok(suggestions)
    }

    /// T4-04, "confirmação explícita": aplica um perfil sugerido — chame
    /// isto só depois do usuário aceitar a sugestão de `suggest_ignore_profiles`,
    /// nunca automaticamente.
    pub async fn apply_ignore_profile(&self, profile: &nexofs_ignore::Profile) -> Result<(), SyncError> {
        for pattern in profile.patterns {
            self.add_ignore_rule(RuleTier::TechProfile, pattern).await?;
        }
        Ok(())
    }

    /// T4-08/FR-LOC-006: conta itens e bytes de uma subárvore `LocalOnly`
    /// antes de propor trazê-la de volta para o fluxo normal — "estimar
    /// operações, classificar risco" da SPEC §17.5 fica a critério de quem
    /// chama (ex.: acima de N itens/bytes, exigir confirmação extra).
    /// Escopo desta entrega: um item/subárvore específico escolhido pelo
    /// usuário (equivalente a alternar "sincronização seletiva" nesta
    /// pasta), não uma varredura da árvore inteira atrás de toda ocorrência
    /// de um padrão recém-desativado — essa varredura completa dependeria
    /// de indexação além do que a Fase 4 carrega preguiçosamente.
    pub async fn estimate_local_only_subtree(&self, item_id: ItemId) -> Result<MigrationEstimate, SyncError> {
        let mut estimate = MigrationEstimate::default();
        let mut stack = vec![item_id];
        while let Some(current) = stack.pop() {
            let item = self.get_item(current).await?.ok_or(SyncError::NotFound)?;
            estimate.item_count += 1;
            estimate.total_bytes += item.size_bytes;
            if item.kind == ItemKind::Directory {
                for child in self.list_children(current).await? {
                    stack.push(child.item_id);
                }
            }
        }
        Ok(estimate)
    }

    /// T4-08/FR-LOC-006, execução após confirmação explícita: promove um
    /// item `LocalOnly` (e toda sua subárvore já indexada) de volta ao
    /// fluxo normal — cria as pastas remotamente e enfileira upload de cada
    /// arquivo. Usa a mesma prioridade de upload comum (`OperationClass::Upload`)
    /// em vez de uma fila de prioridade dedicada — SPEC §17.5 pede "fila de
    /// baixa prioridade" para não competir com escrita interativa; a
    /// diferenciação fina de prioridade por operação em massa fica para
    /// quando houver volume real para calibrar contra (mesmo raciocínio já
    /// aplicado ao backoff em T3-06).
    pub async fn migrate_local_only_to_normal_sync(&self, item_id: ItemId) -> Result<(), SyncError> {
        let mut stack = vec![item_id];
        while let Some(current) = stack.pop() {
            let Some(item) = self.get_item(current).await? else { continue };
            if item.source_layer != "LOCAL_ONLY" {
                continue;
            }

            let current_s = current.to_string();
            self.store.write(move |tx| tx.execute("UPDATE items SET source_layer = 'LOCAL' WHERE item_id = ?1", [current_s])).await?;

            match item.kind {
                ItemKind::Directory => {
                    for child in self.list_children(current).await? {
                        stack.push(child.item_id);
                    }
                    let priority = OperationClass::RemoteMutation.default_priority();
                    let payload = serde_json::json!({ "parent_item_id": item.parent_item_id.map(|p| p.to_string()), "name": item.name }).to_string();
                    self.enqueue_operation(
                        Some(current),
                        OperationType::CreateDirectory,
                        format!("create_dir:{}:{}", self.ctx.namespace_id, current),
                        priority.0,
                        None,
                        payload,
                    )
                    .await?;
                }
                ItemKind::File => {
                    // O conteúdo real está no overlay, não em `cache/dirty/`
                    // — o pipeline de upload existente só olha para lá
                    // (`snapshot_dirty_for_upload`), então precisa
                    // materializar ali antes de marcar `Dirty`. Reaproveita
                    // `begin_dirty_write` passando o próprio arquivo do
                    // overlay como base a copiar/reflinkar, em vez de
                    // duplicar essa lógica aqui.
                    let cache_object_id = current.to_string();
                    let overlay_path = self.overlay.path_for(&cache_object_id);
                    if overlay_path.is_file() {
                        self.cache.begin_dirty_write(&cache_object_id, Some(&overlay_path)).await?;
                    }

                    let now = now_unix();
                    let current_s = current.to_string();
                    self.store
                        .write(move |tx| {
                            tx.execute(
                                "UPDATE local_states SET sync_state = 'DIRTY', dirty_since = COALESCE(dirty_since, ?1), updated_at = ?1 WHERE item_id = ?2",
                                params![now, current_s],
                            )
                        })
                        .await?;
                    self.stabilize_upload(current).await?;
                    let _ = self.overlay.remove(&cache_object_id);
                }
            }
        }
        Ok(())
    }

    /// T4-08/FR-LOC-005: marca um item já sincronizado (e sua subárvore já
    /// indexada) como `LocalOnly` — o "sim, aplique" depois do usuário
    /// escolher entre manter o remoto intacto (`keep_remote_copy = true`,
    /// só para de sincronizar dali em diante) ou removê-lo de vez
    /// (`keep_remote_copy = false`, enfileira a exclusão remota de verdade).
    /// Cancela qualquer operação ainda pendente para os itens afetados —
    /// não faz sentido subir algo que está prestes a virar local-only.
    pub async fn migrate_normal_sync_to_local_only(&self, item_id: ItemId, keep_remote_copy: bool) -> Result<(), SyncError> {
        let mut stack = vec![item_id];
        while let Some(current) = stack.pop() {
            let Some(item) = self.get_item(current).await? else { continue };
            if item.source_layer == "LOCAL_ONLY" {
                continue;
            }

            self.cancel_pending_operations_for_item(current).await?;

            if item.kind == ItemKind::Directory {
                for child in self.list_children(current).await? {
                    stack.push(child.item_id);
                }
            }

            // "Remover remoto" apaga de verdade, aqui e agora, em vez de
            // passar pelo journal: `DeleteItem` normal termina em
            // `hard_delete_item` quando confirmado (T3-06), que apagaria
            // este item do índice local também — mas a intenção aqui é
            // "sim, apague do remoto, mas mantenha a cópia local" (SPEC
            // §17.5), um resultado que o fluxo padrão de exclusão não
            // produz. Fazer a chamada síncrona também evita uma corrida real:
            // se isto fosse enfileirado e `items.remote_item_id` fosse
            // limpo antes do despacho rodar, `dispatch_delete` releria o
            // item já sem `remote_item_id` e cancelaria a operação sem
            // nunca chamar o provedor.
            if !keep_remote_copy {
                if let Some(remote_item_id) = item.remote_item_id.clone() {
                    self.delete_remote_item_now(remote_item_id, item.remote_version.clone()).await?;
                }
            }

            // Para um arquivo, precisa dos bytes reais no overlay ANTES de
            // desvincular o `remote_item_id` — depois de limpo, não há mais
            // de onde baixar (hidratar depende dele existir).
            if item.kind == ItemKind::File {
                let cache_object_id = current.to_string();
                if !self.overlay.exists(&cache_object_id) {
                    match self.open_and_hydrate(current).await {
                        Ok(hydrated_path) => {
                            let _ = self.overlay.create_empty(&cache_object_id).await;
                            let _ = tokio::fs::copy(&hydrated_path, self.overlay.path_for(&cache_object_id)).await;
                            let _ = self.cache.remove_dirty(&cache_object_id);
                        }
                        Err(_) => {
                            let _ = self.overlay.create_empty(&cache_object_id).await;
                        }
                    }
                }
                let now = now_unix();
                let current_s = current.to_string();
                self.store
                    .write(move |tx| {
                        tx.execute(
                            "INSERT INTO local_states (item_id, hydration_state, pin_state, sync_state, cache_object_id, last_access_at, open_handle_count, updated_at) \
                             VALUES (?1, 'HYDRATED', 'AVAILABLE_LOCALLY', 'LOCAL_ONLY', ?1, ?2, 0, ?2) \
                             ON CONFLICT(item_id) DO UPDATE SET sync_state = 'LOCAL_ONLY', cache_object_id = excluded.cache_object_id, updated_at = excluded.updated_at",
                            params![current_s, now],
                        )
                    })
                    .await?;
            }

            // Independentemente da escolha, este cliente para de rastrear o
            // vínculo remoto agora — "manter remoto" preserva o objeto do
            // outro lado, mas as duas cópias seguem vidas separadas dali em
            // diante (é exatamente isso que `LocalOnly` significa).
            let current_s = current.to_string();
            self.store
                .write(move |tx| tx.execute("UPDATE items SET source_layer = 'LOCAL_ONLY', remote_item_id = NULL WHERE item_id = ?1", [current_s]))
                .await?;
        }
        Ok(())
    }
}
