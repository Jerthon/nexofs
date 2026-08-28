//! T5-10/SPEC §23.3 — o que o núcleo sabe sobre si mesmo para o pacote de
//! diagnóstico: nada aqui é conteúdo de arquivo nem segredo (o próprio
//! `SecretToken` já redige tudo o que passa por `Debug`/`Display`, então a
//! "redação de logs" da SPEC já vale desde a emissão, não como um filtro
//! aplicado depois). O resto do pacote (versão do binário, distro/kernel/
//! desktop/session, estado systemd/FUSE) não é responsabilidade do núcleo —
//! `nexofsd`/`nexofs-local-api` sabem disso, `SyncCore` não.

use super::SyncCore;
use crate::error::SyncError;
use nexofs_content_cache::DiskPressureLevel;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Serialize)]
pub struct NamespaceDiagnostics {
    pub schema_version: i64,
    pub sqlite_integrity_ok: bool,
    pub pending_operations_by_state: HashMap<&'static str, u64>,
    pub open_conflicts: u64,
    pub hydrated_items: u64,
    pub hydrated_bytes: u64,
    pub disk_pressure: String,
}

impl SyncCore {
    pub async fn diagnostics_snapshot(&self) -> Result<NamespaceDiagnostics, SyncError> {
        let (schema_version, integrity): (i64, String) = self
            .store
            .read(|conn| {
                let schema_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
                let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                Ok((schema_version, integrity))
            })
            .await?;

        let pending = self.pending_operations().await?;
        let mut pending_operations_by_state: HashMap<&'static str, u64> = HashMap::new();
        for op in &pending {
            *pending_operations_by_state.entry(operation_state_label(op.state)).or_insert(0) += 1;
        }

        let open_conflicts = self.list_conflicts().await?.len() as u64;
        let cache_stats = self.cache_stats().await?;
        let disk_pressure = self.disk_pressure().unwrap_or(DiskPressureLevel::Normal);

        Ok(NamespaceDiagnostics {
            schema_version,
            sqlite_integrity_ok: integrity.eq_ignore_ascii_case("ok"),
            pending_operations_by_state,
            open_conflicts,
            hydrated_items: cache_stats.hydrated_items,
            hydrated_bytes: cache_stats.hydrated_bytes,
            disk_pressure: format!("{disk_pressure:?}"),
        })
    }
}

fn operation_state_label(state: nexofs_domain::states::OperationState) -> &'static str {
    use nexofs_domain::states::OperationState::*;
    match state {
        Pending => "PENDING",
        Running => "RUNNING",
        WaitingRetry => "WAITING_RETRY",
        WaitingNetwork => "WAITING_NETWORK",
        WaitingAuthentication => "WAITING_AUTHENTICATION",
        BlockedByConflict => "BLOCKED_BY_CONFLICT",
        Completed => "COMPLETED",
        Cancelled => "CANCELLED",
        FailedPermanent => "FAILED_PERMANENT",
    }
}
