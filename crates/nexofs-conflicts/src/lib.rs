//! Detecção de conflitos remoto/local. SPEC §18. Não conhece SQLite nem
//! journal — apenas a taxonomia (`SPEC §18.1`, já em `nexofs_domain::states`)
//! e a classificação pura de quando um conflito existe; a persistência em
//! `conflicts` e a orquestração (bloquear operação, preservar snapshot dirty)
//! ficam em `nexofs-sync-core`, que já possui acesso ao índice local.

use nexofs_domain::states::{ConflictResolution, ConflictType, OperationType};
use nexofs_provider_api::ProviderErrorKind;

/// Classifica uma falha do provedor ocorrida ao despachar `operation_type`
/// contra um item local (SPEC §18.2: conflito quando `local_dirty == true
/// AND current_remote_version != base_remote_version`, ou quando a operação
/// não pode ser mapeada sem descartar dados). `None` quando o erro não é um
/// conflito (rede, rate limit, etc.) — quem chama trata esses casos como
/// retry/falha comum, sem envolver `nexofs-conflicts`.
///
/// Cobre apenas os três tipos de T3-08 (`ContentChangedBothSides`,
/// `RemoteDeletedLocalModified`, `LocalDeletedRemoteModified`); os demais
/// tipos de `ConflictType` (renomeio/case/overlay/nome inválido) dependem do
/// Local-Only Overlay e da fila de exclusões da Fase 4 (T4-07 em diante).
pub fn classify_provider_error(operation_type: OperationType, error_kind: &ProviderErrorKind) -> Option<ConflictType> {
    match operation_type {
        OperationType::DeleteItem => {
            matches!(error_kind, ProviderErrorKind::VersionConflict).then_some(ConflictType::LocalDeletedRemoteModified)
        }
        OperationType::UploadFile | OperationType::MoveItem | OperationType::RenameItem => match error_kind {
            ProviderErrorKind::VersionConflict => Some(ConflictType::ContentChangedBothSides),
            ProviderErrorKind::NotFound => Some(ConflictType::RemoteDeletedLocalModified),
            _ => None,
        },
        _ => None,
    }
}

/// SPEC §18.4 "KeepBoth": `<base> (conflito local YYYY-MM-DD HH-mm[-n]).<ext>`
/// — preserva a extensão, evita colisão com `existing_names` (tentando
/// `-2`, `-3`, ... até achar um nome livre) e nunca ultrapassa 255 bytes
/// (limite comum de nome de arquivo em sistemas de arquivo Linux), cortando
/// a parte do nome original se precisar.
pub fn generate_keep_both_name(original_name: &str, timestamp: chrono::DateTime<chrono::Utc>, existing_names: &std::collections::HashSet<String>) -> String {
    const MAX_NAME_BYTES: usize = 255;

    let (stem, ext) = match original_name.rsplit_once('.') {
        // Sem extensão, ou "arquivo." (ponto no fim) tratado como sem
        // extensão de verdade — não há nada útil para preservar depois dele.
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, Some(ext)),
        _ => (original_name, None),
    };

    let suffix_base = format!(" (conflito local {})", timestamp.format("%Y-%m-%d %H-%M"));
    let build = |suffix: &str, stem: &str| -> String {
        match ext {
            Some(ext) => format!("{stem}{suffix}.{ext}"),
            None => format!("{stem}{suffix}"),
        }
    };

    let mut n = 1u32;
    loop {
        let suffix = if n == 1 { suffix_base.clone() } else { format!("{suffix_base}-{n}") };
        let fixed_len = suffix.len() + ext.map(|e| e.len() + 1).unwrap_or(0);
        let max_stem_len = MAX_NAME_BYTES.saturating_sub(fixed_len);
        let truncated_stem = truncate_to_byte_len(stem, max_stem_len);
        let candidate = build(&suffix, truncated_stem);
        if !existing_names.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

fn truncate_to_byte_len(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn conflict_type_to_sql(t: ConflictType) -> &'static str {
    match t {
        ConflictType::ContentChangedBothSides => "CONTENT_CHANGED_BOTH_SIDES",
        ConflictType::RemoteDeletedLocalModified => "REMOTE_DELETED_LOCAL_MODIFIED",
        ConflictType::LocalDeletedRemoteModified => "LOCAL_DELETED_REMOTE_MODIFIED",
        ConflictType::RenameCollision => "RENAME_COLLISION",
        ConflictType::MoveCollision => "MOVE_COLLISION",
        ConflictType::CaseCollision => "CASE_COLLISION",
        ConflictType::LocalOnlyRemoteCollision => "LOCAL_ONLY_REMOTE_COLLISION",
        ConflictType::ParentDeleted => "PARENT_DELETED",
        ConflictType::UnsupportedName => "UNSUPPORTED_NAME",
    }
}

pub fn conflict_type_from_sql(value: &str) -> Option<ConflictType> {
    Some(match value {
        "CONTENT_CHANGED_BOTH_SIDES" => ConflictType::ContentChangedBothSides,
        "REMOTE_DELETED_LOCAL_MODIFIED" => ConflictType::RemoteDeletedLocalModified,
        "LOCAL_DELETED_REMOTE_MODIFIED" => ConflictType::LocalDeletedRemoteModified,
        "RENAME_COLLISION" => ConflictType::RenameCollision,
        "MOVE_COLLISION" => ConflictType::MoveCollision,
        "CASE_COLLISION" => ConflictType::CaseCollision,
        "LOCAL_ONLY_REMOTE_COLLISION" => ConflictType::LocalOnlyRemoteCollision,
        "PARENT_DELETED" => ConflictType::ParentDeleted,
        "UNSUPPORTED_NAME" => ConflictType::UnsupportedName,
        _ => return None,
    })
}

pub fn conflict_resolution_to_sql(r: ConflictResolution) -> &'static str {
    match r {
        ConflictResolution::KeepLocal => "KEEP_LOCAL",
        ConflictResolution::KeepRemote => "KEEP_REMOTE",
        ConflictResolution::KeepBoth => "KEEP_BOTH",
        ConflictResolution::SaveLocalElsewhere => "SAVE_LOCAL_ELSEWHERE",
        ConflictResolution::DismissTemporarily => "DISMISS_TEMPORARILY",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_version_conflict_is_content_changed_both_sides() {
        assert_eq!(
            classify_provider_error(OperationType::UploadFile, &ProviderErrorKind::VersionConflict),
            Some(ConflictType::ContentChangedBothSides)
        );
    }

    #[test]
    fn upload_not_found_is_remote_deleted_local_modified() {
        assert_eq!(
            classify_provider_error(OperationType::UploadFile, &ProviderErrorKind::NotFound),
            Some(ConflictType::RemoteDeletedLocalModified)
        );
    }

    #[test]
    fn delete_version_conflict_is_local_deleted_remote_modified() {
        assert_eq!(
            classify_provider_error(OperationType::DeleteItem, &ProviderErrorKind::VersionConflict),
            Some(ConflictType::LocalDeletedRemoteModified)
        );
    }

    #[test]
    fn delete_not_found_is_not_a_conflict() {
        // Já tratado como sucesso pelo dispatcher antes de chegar aqui — o
        // objetivo local (remover) já foi alcançado.
        assert_eq!(classify_provider_error(OperationType::DeleteItem, &ProviderErrorKind::NotFound), None);
    }

    #[test]
    fn transient_errors_are_never_conflicts() {
        assert_eq!(classify_provider_error(OperationType::UploadFile, &ProviderErrorKind::Network), None);
    }

    fn ts() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-26T14:05:00Z").unwrap().with_timezone(&chrono::Utc)
    }

    #[test]
    fn keep_both_preserves_extension_and_embeds_the_timestamp() {
        let name = generate_keep_both_name("relatorio.xlsx", ts(), &Default::default());
        assert_eq!(name, "relatorio (conflito local 2026-08-26 14-05).xlsx");
    }

    #[test]
    fn keep_both_handles_a_name_with_no_extension() {
        let name = generate_keep_both_name("Makefile", ts(), &Default::default());
        assert_eq!(name, "Makefile (conflito local 2026-08-26 14-05)");
    }

    #[test]
    fn keep_both_avoids_colliding_with_an_existing_name() {
        let mut existing = std::collections::HashSet::new();
        existing.insert("a (conflito local 2026-08-26 14-05).txt".to_string());
        existing.insert("a (conflito local 2026-08-26 14-05)-2.txt".to_string());
        let name = generate_keep_both_name("a.txt", ts(), &existing);
        assert_eq!(name, "a (conflito local 2026-08-26 14-05)-3.txt");
    }

    #[test]
    fn keep_both_never_exceeds_255_bytes() {
        let long_name = format!("{}.txt", "a".repeat(300));
        let name = generate_keep_both_name(&long_name, ts(), &Default::default());
        assert!(name.len() <= 255, "nome gerado tem {} bytes: {name}", name.len());
        assert!(name.ends_with(".txt"), "extensão precisa ser preservada mesmo truncando: {name}");
    }
}
