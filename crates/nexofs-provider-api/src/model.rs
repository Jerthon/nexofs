//! Modelo de dados neutro trocado entre núcleo e adaptadores.
//! PRD §9.3 / SPEC §9-10 (colunas de `items`, `namespaces`).

use nexofs_domain::RemoteItemId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ItemKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteItem {
    pub remote_item_id: RemoteItemId,
    pub parent_remote_item_id: Option<RemoteItemId>,
    pub name: String,
    pub kind: ItemKind,
    pub size_bytes: u64,
    pub mime_type: Option<String>,
    /// Versão geral do item/metadados quando o provedor distingue (SPEC §11.3).
    pub remote_version: Option<String>,
    /// Versão do conteúdo quando distinguível da versão de metadados.
    pub remote_content_version: Option<String>,
    pub remote_modified_at_unix: Option<i64>,
    pub remote_created_at_unix: Option<i64>,
    /// Metadados específicos do provedor, opacos ao núcleo (FR-MC-001).
    pub provider_metadata_json: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NamespaceKind {
    Personal,
    Shared,
    SiteLibrary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteNamespace {
    pub remote_namespace_id: String,
    pub display_name: String,
    pub kind: NamespaceKind,
}

/// Página de listagem — paginação nativa do provedor, nunca a árvore inteira
/// (FR-IDX-003, "diretório com 100 mil filhos sem carregar todos em memória").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePage<T> {
    pub items: Vec<T>,
    pub next_page_token: Option<String>,
}

/// Cursor opaco — DEVE ser persistido exatamente como recebido, nunca
/// reconstruído ou inspecionado pelo núcleo (SPEC §14.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeCursor(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteChange {
    Upserted(RemoteItem),
    Deleted { remote_item_id: RemoteItemId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePage {
    pub changes: Vec<RemoteChange>,
    pub next_cursor: ChangeCursor,
    /// `true` quando há mais páginas a buscar antes do cursor ficar corrente.
    pub has_more: bool,
}

/// Intervalo de bytes para download por range (FR-HYD-005), quando suportado.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    /// Inclusivo, como em HTTP `Range`.
    pub end: u64,
}
