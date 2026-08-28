//! Pressão de disco (T4-14, SPEC §19.4). Consulta o espaço livre real do
//! sistema de arquivos onde o cache vive (`statvfs`) — não uma estimativa a
//! partir do que o NexoFS acha que já usou, porque outros programas também
//! escrevem no mesmo disco.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiskPressureLevel {
    Normal,
    Warning,
    Critical,
    Emergency,
}

/// Limiares — percentual livre E um piso absoluto, o que disparar primeiro:
/// um disco de 8 TB a 10% livre ainda tem centenas de GB de fôlego (não é
/// `Warning` de verdade), enquanto um disco pequeno a 10% livre pode já
/// estar na casa de poucos GB (é, sim, `Warning`).
const EMERGENCY_MIN_FREE_BYTES: u64 = 200 * 1024 * 1024;
const EMERGENCY_MIN_FREE_RATIO: f64 = 0.01;
const CRITICAL_MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;
const CRITICAL_MIN_FREE_RATIO: f64 = 0.05;
const WARNING_MIN_FREE_RATIO: f64 = 0.15;

/// Nível de pressão do sistema de arquivos que contém `path`. `path` (ou o
/// ancestral existente mais próximo — `clean/` pode ainda não ter sido
/// criado na primeira execução) é consultado via `statvfs`.
pub fn disk_pressure_level(path: &Path) -> std::io::Result<DiskPressureLevel> {
    let existing = path.ancestors().find(|p| p.exists()).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "nenhum ancestral existe"))?;
    let stats = nix::sys::statvfs::statvfs(existing).map_err(std::io::Error::from)?;
    let block_size = stats.block_size() as u64;
    let total = block_size.saturating_mul(stats.blocks() as u64);
    let free = block_size.saturating_mul(stats.blocks_available() as u64);
    let free_ratio = if total == 0 { 1.0 } else { free as f64 / total as f64 };

    Ok(if free <= EMERGENCY_MIN_FREE_BYTES || free_ratio <= EMERGENCY_MIN_FREE_RATIO {
        DiskPressureLevel::Emergency
    } else if free <= CRITICAL_MIN_FREE_BYTES || free_ratio <= CRITICAL_MIN_FREE_RATIO {
        DiskPressureLevel::Critical
    } else if free_ratio <= WARNING_MIN_FREE_RATIO {
        DiskPressureLevel::Warning
    } else {
        DiskPressureLevel::Normal
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_directory_reports_some_level_without_erroring() {
        let dir = tempfile::tempdir().unwrap();
        // Não afirma qual nível — depende da máquina que roda o teste — só
        // que a chamada real ao `statvfs` funciona e devolve algo válido.
        let level = disk_pressure_level(dir.path()).unwrap();
        assert!(matches!(
            level,
            DiskPressureLevel::Normal | DiskPressureLevel::Warning | DiskPressureLevel::Critical | DiskPressureLevel::Emergency
        ));
    }

    #[test]
    fn a_not_yet_created_subdirectory_falls_back_to_the_nearest_existing_ancestor() {
        // `clean/` pode não existir ainda na primeira execução (só é criado
        // na primeira hidratação) — a consulta não pode falhar por isso.
        let dir = tempfile::tempdir().unwrap();
        let not_yet_created = dir.path().join("cache").join("clean");
        assert!(!not_yet_created.exists());
        disk_pressure_level(&not_yet_created).unwrap();
    }
}
