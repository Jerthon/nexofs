//! Fixação (T4-10/T4-11, SPEC §12.2, FR-PIN-001 a 004). `PinState` já
//! existia desde a Fase 0 (`nexofs_domain::states`) mas nada nunca escrevia
//! `PINNED` em `local_states.pin_state` — a proteção de `enforce_cache_quota`
//! contra eviction de item fixado (T4-11) já existe desde a correção feita
//! junto com T4-05; aqui só falta o próprio ato de fixar.

use super::SyncCore;
use crate::error::SyncError;
use crate::queries::now_unix;
use nexofs_api_governor::OperationClass;
use nexofs_domain::states::PinState;
use nexofs_domain::ItemId;
use nexofs_provider_api::ItemKind;
use rusqlite::{params, OptionalExtension};
use std::sync::Arc;

pub(crate) fn pin_state_to_sql(state: PinState) -> &'static str {
    match state {
        PinState::OnlineOnly => "ONLINE_ONLY",
        PinState::AvailableLocally => "AVAILABLE_LOCALLY",
        PinState::Pinned => "PINNED",
    }
}

impl SyncCore {
    /// FR-PIN-001: aplica um dos três estados a um único item — cria a
    /// linha de `local_states` se ainda não existir (um item puramente
    /// remoto nunca tocado localmente ainda não tem uma).
    pub async fn set_pin_state(&self, item_id: ItemId, pin_state: PinState) -> Result<(), SyncError> {
        let item_id_s = item_id.to_string();
        let state_s = pin_state_to_sql(pin_state);
        let now = now_unix();
        self.store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO local_states (item_id, hydration_state, pin_state, sync_state, last_access_at, open_handle_count, updated_at) \
                     VALUES (?1, 'PLACEHOLDER', ?2, 'CLEAN', ?3, 0, ?3) \
                     ON CONFLICT(item_id) DO UPDATE SET pin_state = excluded.pin_state, updated_at = excluded.updated_at",
                    params![item_id_s, state_s, now],
                )
            })
            .await?;
        Ok(())
    }

    pub async fn pin_state_of(&self, item_id: ItemId) -> Result<PinState, SyncError> {
        let item_id_s = item_id.to_string();
        let value: Option<String> = self
            .store
            .read(move |conn| {
                conn.query_row("SELECT pin_state FROM local_states WHERE item_id = ?1", [item_id_s], |row| row.get(0)).optional()
            })
            .await?;
        Ok(match value.as_deref() {
            Some("PINNED") => PinState::Pinned,
            Some("AVAILABLE_LOCALLY") => PinState::AvailableLocally,
            _ => PinState::OnlineOnly,
        })
    }

    /// FR-PIN-002: fixa `item_id` e, se for uma pasta, toda a subárvore —
    /// "sem bloquear a UI": a chamada em si só agenda e retorna na hora; a
    /// varredura (listando pastas ainda não visitadas, se preciso) e a
    /// HIDRATAÇÃO de cada arquivo descendente ainda não baixado acontecem
    /// numa tarefa de fundo separada. A hidratação usa `BackgroundIndex`
    /// (a prioridade mais baixa de leitura) para nunca competir com um
    /// download interativo real; a listagem de pastas ainda não visitadas
    /// reaproveita `list_children`/`ensure_children_loaded` como estão, que
    /// hoje sempre usam `InteractiveMetadata` — refinar a prioridade também
    /// desse lado fica para quando `ensure_children_loaded` for
    /// parametrizado (mesmo padrão já aplicado à hidratação aqui).
    pub fn pin_recursive(self: &Arc<Self>, item_id: ItemId) {
        let core = self.clone();
        self.runtime_handle.spawn(async move {
            if let Err(err) = core.set_pin_state(item_id, PinState::Pinned).await {
                tracing::warn!(?err, %item_id, "falha ao fixar item");
                return;
            }
            if let Err(err) = core.pin_recursive_inner(item_id).await {
                tracing::warn!(?err, %item_id, "falha ao fixar recursivamente");
            }
        });
    }

    async fn pin_recursive_inner(&self, root_item_id: ItemId) -> Result<(), SyncError> {
        let Some(root) = self.get_item(root_item_id).await? else {
            return Ok(());
        };
        if root.kind != ItemKind::Directory {
            if root.source_layer != "LOCAL_ONLY" {
                if let Err(err) = self.open_and_hydrate_with_priority(root_item_id, OperationClass::BackgroundIndex).await {
                    tracing::warn!(?err, item_id = %root_item_id, "falha ao hidratar item fixado");
                }
            }
            return Ok(());
        }

        let mut stack = vec![root_item_id];
        while let Some(current) = stack.pop() {
            // SPEC §19.4 item 1: "interromper prefetch/download fixado não
            // iniciado" — cada arquivo ainda não hidratado desta fixação
            // recursiva é exatamente isso; para de tentar mais hidratações
            // (a fixação em si, já marcada, continua valendo).
            if self.disk_pressure().unwrap_or(nexofs_content_cache::DiskPressureLevel::Normal) == nexofs_content_cache::DiskPressureLevel::Emergency {
                tracing::warn!("pressão de disco em EMERGENCY — interrompendo hidratação de descendentes fixados ainda não iniciada");
                return Ok(());
            }

            let Ok(children) = self.list_children(current).await else { continue };
            for child in children {
                self.set_pin_state(child.item_id, PinState::Pinned).await?;
                match child.kind {
                    ItemKind::Directory => stack.push(child.item_id),
                    ItemKind::File if child.source_layer != "LOCAL_ONLY" => {
                        if let Err(err) = self.open_and_hydrate_with_priority(child.item_id, OperationClass::BackgroundIndex).await {
                            tracing::warn!(?err, item_id = %child.item_id, "falha ao hidratar descendente fixado");
                        }
                    }
                    ItemKind::File => {}
                }
            }
        }
        Ok(())
    }
}
