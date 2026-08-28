//! Conversão DTO do Graph → modelo neutro (`nexofs-provider-api`).

use crate::dto::GraphDriveItem;
use nexofs_domain::RemoteItemId;
use nexofs_provider_api::{ItemKind, RemoteItem};

fn parse_rfc3339_to_unix(value: &Option<String>) -> Option<i64> {
    value
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
}

pub fn map_drive_item(item: GraphDriveItem) -> RemoteItem {
    let kind = if item.folder.is_some() {
        ItemKind::Directory
    } else {
        ItemKind::File
    };

    // O Graph expõe duas noções de versão: `eTag` (metadados) e `cTag`
    // (conteúdo) — mapeadas 1:1 para `remote_version`/`remote_content_version`
    // (SPEC §6.2, PRD §11.3).
    RemoteItem {
        remote_item_id: RemoteItemId::from(item.id),
        parent_remote_item_id: item
            .parent_reference
            .and_then(|p| p.id)
            .map(RemoteItemId::from),
        name: item.name.unwrap_or_default(),
        kind,
        size_bytes: item.size.unwrap_or(0),
        mime_type: None,
        remote_version: item.etag,
        remote_content_version: item.ctag,
        remote_modified_at_unix: parse_rfc3339_to_unix(&item.last_modified_date_time),
        remote_created_at_unix: parse_rfc3339_to_unix(&item.created_date_time),
        provider_metadata_json: None,
    }
}
