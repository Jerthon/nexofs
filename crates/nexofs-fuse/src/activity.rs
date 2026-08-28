//! Detecção de atividade "browser-aware" (FR-ACT-002/003/005). O FUSE
//! entrega o PID do processo chamador em cada request; correlacionamos com
//! `/proc/{pid}/comm` para diferenciar um gerenciador de arquivos real de
//! um thumbnailer/indexador em segundo plano.
//!
//! Limitação reconhecida na própria SPEC (§6.4): não existe forma portável
//! de detectar "janela visível" — o fallback é classificar por nome de
//! processo, com viés a favor de tratar o desconhecido como interativo (um
//! falso positivo aqui só custa uma consulta incremental extra; um falso
//! negativo esconderia atividade real do usuário).

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityPolicy {
    /// FR-ACT-005: só processos de gerenciador de arquivos conhecidos
    /// disparam verificação incremental — padrão.
    BrowserAware,
    /// Qualquer acesso interativo (exceto indexadores conhecidos) dispara.
    AnyAccess,
    /// Nunca dispara por atividade — só o timer periódico e o refresh manual.
    Manual,
}

impl ActivityPolicy {
    pub fn from_env() -> Self {
        match std::env::var("NEXOFS_ACTIVITY_POLICY").as_deref() {
            Ok("any-access") => Self::AnyAccess,
            Ok("manual") => Self::Manual,
            _ => Self::BrowserAware,
        }
    }
}

/// Nomes vistos em `/proc/{pid}/comm` (15 bytes, truncado pelo kernel) para
/// gerenciadores de arquivos comuns em GNOME/KDE (PRD §13.1).
const KNOWN_FILE_MANAGERS: &[&str] = &["nautilus", "dolphin", "nemo", "thunar", "pcmanfm", "pcmanfm-qt", "files"];

/// Thumbnailers e indexadores conhecidos — nunca devem manter refresh
/// contínuo ativo (FR-ACT-003).
const KNOWN_BACKGROUND_INDEXERS: &[&str] = &[
    "tracker-extract",
    "tracker-miner-f",
    "baloo_file",
    "baloo_file_extr",
    "tumblerd",
    "gvfsd-metadata",
    "gvfs-udisks2-vo",
    "evince-thumbnai",
];

fn process_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Decide se esta chamada deve contar como atividade interativa, segundo a
/// política ativa. `pid = 0` (contexto sem processo associado, ex.: kernel)
/// nunca é tratado como interativo.
pub fn is_interactive(pid: u32, policy: ActivityPolicy) -> bool {
    if pid == 0 {
        return false;
    }

    match policy {
        ActivityPolicy::Manual => false,
        ActivityPolicy::AnyAccess => !matches!(process_comm(pid), Some(name) if is_background_indexer(&name)),
        ActivityPolicy::BrowserAware => match process_comm(pid) {
            Some(name) if is_background_indexer(&name) => false,
            Some(name) if is_known_file_manager(&name) => true,
            // Nome desconhecido (editor, terminal, IDE...): trata como
            // interativo — o custo de um falso positivo é uma consulta a
            // mais, nunca perda de atualização.
            Some(_) | None => true,
        },
    }
}

fn is_known_file_manager(name: &str) -> bool {
    KNOWN_FILE_MANAGERS.iter().any(|known| name.eq_ignore_ascii_case(known))
}

fn is_background_indexer(name: &str) -> bool {
    KNOWN_BACKGROUND_INDEXERS.iter().any(|known| name.eq_ignore_ascii_case(known))
}

pub fn cached_policy() -> ActivityPolicy {
    static POLICY: OnceLock<ActivityPolicy> = OnceLock::new();
    *POLICY.get_or_init(ActivityPolicy::from_env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_policy_never_interactive() {
        assert!(!is_interactive(std::process::id(), ActivityPolicy::Manual));
    }

    #[test]
    fn zero_pid_is_never_interactive() {
        assert!(!is_interactive(0, ActivityPolicy::BrowserAware));
        assert!(!is_interactive(0, ActivityPolicy::AnyAccess));
    }

    #[test]
    fn unknown_process_defaults_to_interactive_under_browser_aware() {
        // O processo de teste atual (ex.: o binário de testes do cargo) não
        // está nas listas conhecidas — deve ser tratado como interativo.
        assert!(is_interactive(std::process::id(), ActivityPolicy::BrowserAware));
    }

    #[test]
    fn known_indexer_name_is_never_interactive_regardless_of_policy() {
        assert!(!is_background_indexer("nautilus"));
        assert!(is_background_indexer("tracker-extract"));
        assert!(is_known_file_manager("dolphin"));
    }
}
