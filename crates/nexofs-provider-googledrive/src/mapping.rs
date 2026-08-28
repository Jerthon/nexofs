//! Conversão DTO da Drive API → modelo neutro (`nexofs-provider-api`).

use crate::dto::{GoogleFile, FOLDER_MIME_TYPE};
use nexofs_domain::RemoteItemId;
use nexofs_provider_api::{ItemKind, RemoteItem};

fn parse_rfc3339_to_unix(value: &Option<String>) -> Option<i64> {
    value
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
}

pub fn map_file(item: GoogleFile) -> RemoteItem {
    let kind = if item.mime_type.as_deref() == Some(FOLDER_MIME_TYPE) { ItemKind::Directory } else { ItemKind::File };

    // Drive permite múltiplos pais (compartilhamento em mais de uma pasta);
    // o modelo do NexoFS assume hierarquia de árvore única (mesma suposição
    // do OneDrive) — usamos só o primeiro, uma simplificação aceitável para
    // o caso comum (arquivo pertence a exatamente uma pasta).
    let parent_remote_item_id = item.parents.and_then(|mut parents| if parents.is_empty() { None } else { Some(parents.remove(0)) }).map(RemoteItemId::from);

    RemoteItem {
        remote_item_id: RemoteItemId::from(item.id),
        parent_remote_item_id,
        name: item.name.unwrap_or_default(),
        kind,
        size_bytes: item.size.and_then(|s| s.parse().ok()).unwrap_or(0),
        mime_type: item.mime_type,
        remote_version: item.version.clone(),
        // Drive não distingue versão de metadados de versão de conteúdo do
        // jeito que o Graph faz com eTag/cTag — `md5Checksum` é o mais
        // próximo de "versão de conteúdo" disponível (muda só quando os
        // bytes mudam, ao contrário de `version`, que também avança em
        // mudanças de metadados como renomear).
        remote_content_version: item.md5_checksum,
        remote_modified_at_unix: parse_rfc3339_to_unix(&item.modified_time),
        remote_created_at_unix: parse_rfc3339_to_unix(&item.created_time),
        provider_metadata_json: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_file() -> GoogleFile {
        GoogleFile {
            id: "file-1".to_string(),
            name: Some("relatorio.pdf".to_string()),
            mime_type: Some("application/pdf".to_string()),
            size: Some("2048".to_string()),
            version: Some("7".to_string()),
            md5_checksum: Some("abc123".to_string()),
            parents: Some(vec!["parent-1".to_string()]),
            modified_time: Some("2026-01-15T10:30:00.000Z".to_string()),
            created_time: Some("2026-01-01T00:00:00.000Z".to_string()),
            trashed: false,
        }
    }

    #[test]
    fn maps_a_regular_file() {
        let item = map_file(base_file());
        assert_eq!(item.remote_item_id.as_ref(), "file-1");
        assert_eq!(item.name, "relatorio.pdf");
        assert_eq!(item.kind, ItemKind::File);
        // Particularidade real da API v3: `size` chega como string no JSON.
        assert_eq!(item.size_bytes, 2048);
        assert_eq!(item.remote_version.as_deref(), Some("7"));
        assert_eq!(item.remote_content_version.as_deref(), Some("abc123"));
        assert_eq!(item.parent_remote_item_id.map(|id| id.0), Some("parent-1".to_string()));
        assert!(item.remote_modified_at_unix.is_some());
        assert!(item.remote_created_at_unix.is_some());
    }

    #[test]
    fn detects_a_folder_by_mime_type() {
        let mut folder = base_file();
        folder.mime_type = Some(FOLDER_MIME_TYPE.to_string());
        folder.size = None;
        let item = map_file(folder);
        assert_eq!(item.kind, ItemKind::Directory);
        assert_eq!(item.size_bytes, 0);
    }

    #[test]
    fn a_top_level_item_with_no_parents_has_no_parent_id() {
        let mut file = base_file();
        file.parents = Some(vec![]);
        let item = map_file(file);
        assert_eq!(item.parent_remote_item_id, None);
    }

    #[test]
    fn a_file_with_only_the_first_of_multiple_parents_is_used() {
        let mut file = base_file();
        file.parents = Some(vec!["first-parent".to_string(), "second-parent".to_string()]);
        let item = map_file(file);
        assert_eq!(item.parent_remote_item_id.map(|id| id.0), Some("first-parent".to_string()));
    }

    #[test]
    fn an_unparseable_size_falls_back_to_zero_instead_of_panicking() {
        let mut file = base_file();
        file.size = Some("not-a-number".to_string());
        let item = map_file(file);
        assert_eq!(item.size_bytes, 0);
    }
}
