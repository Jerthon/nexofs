//! Pressão de disco (T4-14, SPEC §19.4) — a consulta real (`statvfs`) mora
//! em `nexofs-content-cache`; aqui só a reação: o que fazer em cada nível.

use super::SyncCore;
use crate::error::SyncError;
pub use nexofs_content_cache::DiskPressureLevel;

/// Acima deste tamanho, uma hidratação é "grande" o bastante para ser
/// recusada em `Emergency` (SPEC §19.4 item 3) em vez de arriscar encher o
/// disco por completo no meio do download.
const LARGE_HYDRATION_BYTES: u64 = 200 * 1024 * 1024;

impl SyncCore {
    /// Nível de pressão de disco atual — consulta o sistema de arquivos
    /// real onde o cache deste namespace vive.
    pub fn disk_pressure(&self) -> Result<DiskPressureLevel, SyncError> {
        Ok(nexofs_content_cache::disk_pressure_level(self.cache.root_dir())?)
    }

    /// Reage ao nível atual (SPEC §19.4): `Critical`/`Emergency` evictam
    /// tudo que já é elegível agora mesmo (mesma proteção de T4-11 para
    /// fixado/dirty/conflito/local-only — nada demais é sacrificado só por
    /// causa da pressão), `Warning` só alerta. Retorna o nível observado,
    /// para quem chama (o loop de manutenção periódica do daemon) decidir
    /// se/como notificar o usuário (item 5 do SPEC).
    pub async fn handle_disk_pressure(&self, max_bytes: u64) -> Result<DiskPressureLevel, SyncError> {
        let level = self.disk_pressure()?;
        {
            let mut last = self.last_disk_pressure.lock().expect("mutex de pressão de disco nunca envenena");
            if *last != level {
                *last = level;
                self.event_bus.publish(crate::events::SyncEvent::CachePressureChanged {
                    namespace_id: self.ctx.namespace_id,
                    level: format!("{level:?}"),
                });
            }
        }
        match level {
            DiskPressureLevel::Emergency => {
                tracing::error!(namespace_id = %self.ctx.namespace_id, "pressão de disco em EMERGENCY — evictando tudo elegível agora");
                self.enforce_cache_quota(0).await?;
            }
            DiskPressureLevel::Critical => {
                tracing::warn!(namespace_id = %self.ctx.namespace_id, "pressão de disco CRITICAL — evictando acima da quota configurada");
                self.enforce_cache_quota(max_bytes).await?;
            }
            DiskPressureLevel::Warning => {
                tracing::warn!(namespace_id = %self.ctx.namespace_id, "pressão de disco em WARNING");
            }
            DiskPressureLevel::Normal => {}
        }
        Ok(level)
    }

    /// SPEC §19.4 item 3: impede hidratações grandes quando a pressão está
    /// em `Emergency` — chamado por `open_and_hydrate_with_priority` antes
    /// de abrir a conexão de download, nunca depois de já ter gasto banda.
    pub(crate) fn refuse_if_hydration_too_large_for_emergency(&self, size_bytes: u64) -> Result<(), SyncError> {
        if size_bytes < LARGE_HYDRATION_BYTES {
            return Ok(());
        }
        if self.disk_pressure().unwrap_or(DiskPressureLevel::Normal) == DiskPressureLevel::Emergency {
            return Err(SyncError::InvalidOperation("pressão de disco em EMERGENCY — hidratação grande recusada até haver espaço"));
        }
        Ok(())
    }
}
