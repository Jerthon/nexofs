//! DTOs de resposta da Google Drive API v3 e do endpoint de token.
//! Referência: <https://developers.google.com/drive/api/reference/rest/v3>.
//! Nenhum destes tipos atravessa a fronteira do adaptador (FR-MC-001) — o
//! módulo `mapping` os converte para `nexofs_provider_api::model`.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: i64,
}

#[derive(Debug, Deserialize)]
pub struct GoogleErrorBody {
    pub error: GoogleErrorDetail,
}

#[derive(Debug, Deserialize)]
pub struct GoogleErrorDetail {
    #[allow(dead_code)]
    pub code: i32,
    pub message: String,
    #[serde(default)]
    pub errors: Vec<GoogleErrorReason>,
}

#[derive(Debug, Deserialize)]
pub struct GoogleErrorReason {
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct GoogleUserInfo {
    pub id: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

pub const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

/// `size` vem como string na API v3 (convenção do Google para inteiros de
/// 64 bits em JSON, que não tem um tipo inteiro de precisão arbitrária) —
/// por isso `String`, convertido para `u64` em `mapping`. Arquivos nativos
/// do Google Workspace (Docs/Sheets/Slides) não têm `size` nem podem ser
/// baixados via `alt=media` (exigem `files.export` para um formato de
/// exportação) — não implementado nesta entrega, mesmo escopo do OneDrive
/// (que também não cobre SharePoint/bibliotecas de site ainda); ver
/// `GoogleDriveProvider::open_download`.
#[derive(Debug, Deserialize, Default)]
pub struct GoogleFile {
    pub id: String,
    pub name: Option<String>,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    pub size: Option<String>,
    /// Revisão monotônica do arquivo — usada como `remote_version` e para o
    /// precondition "ler-depois-escrever" de `base_remote_version` (ver
    /// nota em `GoogleDriveProvider::check_version_precondition`: a API v3
    /// não documenta um cabeçalho `If-Match`/ETag como o Graph, então o
    /// controle otimista aqui é uma checagem cliente, não atômica no
    /// servidor).
    pub version: Option<String>,
    #[serde(rename = "md5Checksum")]
    pub md5_checksum: Option<String>,
    pub parents: Option<Vec<String>>,
    #[serde(rename = "modifiedTime")]
    pub modified_time: Option<String>,
    #[serde(rename = "createdTime")]
    pub created_time: Option<String>,
    #[serde(default)]
    pub trashed: bool,
}

#[derive(Debug, Deserialize)]
pub struct GoogleFileList {
    pub files: Vec<GoogleFile>,
    #[serde(rename = "nextPageToken")]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GoogleStartPageToken {
    #[serde(rename = "startPageToken")]
    pub start_page_token: String,
}

#[derive(Debug, Deserialize)]
pub struct GoogleChange {
    #[serde(rename = "fileId")]
    pub file_id: String,
    #[serde(default)]
    pub removed: bool,
    pub file: Option<GoogleFile>,
}

#[derive(Debug, Deserialize)]
pub struct GoogleChangeList {
    pub changes: Vec<GoogleChange>,
    #[serde(rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    #[serde(rename = "newStartPageToken")]
    pub new_start_page_token: Option<String>,
}
