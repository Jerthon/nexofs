//! DTOs de resposta da Microsoft Graph e do endpoint de token.
//! Referência: <https://learn.microsoft.com/graph/api/resources/driveitem>.
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
pub struct GraphErrorBody {
    pub error: GraphErrorDetail,
}

#[derive(Debug, Deserialize)]
pub struct GraphErrorDetail {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct GraphUser {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GraphDrive {
    pub id: String,
    #[serde(rename = "driveType")]
    pub drive_type: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct GraphFolderFacet {}

// `hashes`/`state` ainda não são lidos pelo mapeamento (dedup por hash e
// distinção do tipo de exclusão chegam com o conflict engine, Fase 4) —
// mantidos aqui porque documentam o formato real da resposta do Graph.
#[derive(Debug, Deserialize, Default)]
pub struct GraphFileFacet {
    #[allow(dead_code)]
    pub hashes: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
pub struct GraphDeletedFacet {
    #[allow(dead_code)]
    pub state: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GraphParentReference {
    pub id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GraphDriveItem {
    pub id: String,
    pub name: Option<String>,
    pub size: Option<u64>,
    // Bug real encontrado validando a Fase 3 contra o OneDrive de verdade:
    // sem o rename, este campo nunca casava com a chave real do Graph
    // (`eTag`, T maiúsculo) e ficava sempre `None` — `remote_version`
    // nunca era populado, o que desativava silenciosamente o controle
    // otimista de versão (`If-Match`) em toda escrita real (T3-07).
    #[serde(rename = "eTag")]
    pub etag: Option<String>,
    #[serde(rename = "cTag")]
    pub ctag: Option<String>,
    #[serde(rename = "parentReference")]
    pub parent_reference: Option<GraphParentReference>,
    pub folder: Option<GraphFolderFacet>,
    #[allow(dead_code)]
    pub file: Option<GraphFileFacet>,
    pub deleted: Option<GraphDeletedFacet>,
    #[serde(rename = "lastModifiedDateTime")]
    pub last_modified_date_time: Option<String>,
    #[serde(rename = "createdDateTime")]
    pub created_date_time: Option<String>,
    /// Presente (não-null) só no próprio objeto-raiz do drive — é assim que
    /// o Graph o distingue de uma pasta comum chamada "root" (documentação:
    /// facet `root` de `driveItem`). Bug real encontrado validando a Fase 3:
    /// `/root/delta` inclui esse objeto como o primeiro item da própria
    /// página de mudanças; sem checar este facet, `list_changes` o tratava
    /// como uma pasta filha comum e o indexava sob a raiz sintética,
    /// reparentando todo item de nível superior descoberto depois (que
    /// sempre reporta este objeto como pai) para dentro dele.
    pub root: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct GraphUploadSession {
    #[serde(rename = "uploadUrl")]
    pub upload_url: String,
}

#[derive(Debug, Deserialize)]
pub struct GraphChildrenPage {
    pub value: Vec<GraphDriveItem>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GraphDeltaPage {
    pub value: Vec<GraphDriveItem>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}
