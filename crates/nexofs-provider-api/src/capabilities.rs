//! Capacidades declaradas pelo adaptador. SPEC §5.2.
//!
//! O núcleo NUNCA assume delta, ranges ou hashes — cada estratégia opcional
//! é escolhida em tempo de execução a partir destes campos (FR-MC-002).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HashAlgorithm {
    Sha1,
    Sha256,
    Crc32,
    QuickXorHash,
    /// Google Drive expõe `md5Checksum` no recurso `files` (Fase 7, T7-02).
    Md5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CaseSensitivity {
    Sensitive,
    Insensitive,
    InsensitivePreserving,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub incremental_changes: bool,
    pub latest_cursor_without_full_scan: bool,
    pub push_notifications: bool,
    pub metadata_batch: bool,
    pub resumable_upload: bool,
    pub ranged_download: bool,
    pub stable_item_ids: bool,
    pub content_version: bool,
    pub metadata_version: bool,
    pub remote_hashes: Vec<HashAlgorithm>,
    pub atomic_move: bool,
    pub server_side_copy: bool,
    pub trash: bool,
    pub case_sensitivity: CaseSensitivity,
    pub max_simple_upload_bytes: Option<u64>,
    pub max_item_name_bytes: Option<u32>,
    pub max_path_bytes: Option<u32>,
}
