//! Adaptador Google Drive. SPEC §5 (Fase 7, T7-02).
//!
//! Único crate do workspace autorizado a depender de conceitos da Google
//! Drive API v3 (FR-MC-001) — todo tipo que sai daqui já está convertido
//! para o modelo neutro de `nexofs-provider-api`.
//!
//! Diferenças reais em relação ao adaptador OneDrive (`nexofs-provider-onedrive`),
//! documentadas nos pontos do código onde aparecem:
//! - Não há `client_id` público de referência embutido — precisa vir de
//!   `NEXOFS_GOOGLEDRIVE_CLIENT_ID`/`NEXOFS_GOOGLEDRIVE_CLIENT_SECRET`
//!   (ver `config.rs`).
//! - Drive permite nomes duplicados numa pasta (o Graph não) — `create_directory`/
//!   `upload` verificam colisão de nome no cliente antes de escrever.
//! - Drive não documenta um precondition HTTP tipo `If-Match` para escrita
//!   condicional — o controle otimista de versão aqui é uma checagem
//!   "ler-depois-escrever" (`check_version_precondition`), não atômica no
//!   servidor como o `If-Match` do Graph. Uma janela de corrida estreita
//!   permanece; documentado e aceito como limitação conhecida.
//! - Mover um item exige duas chamadas (descobrir o pai atual, depois
//!   `addParents`/`removeParents`) — o Graph faz isso numa `PATCH` só.
//! - Upload sempre usa sessão resumível (mesmo para arquivos pequenos) —
//!   simplificação deliberada para não implementar upload multipart à mão;
//!   correto, só não é o mais eficiente possível para arquivos minúsculos.
//! - Arquivos nativos do Google Workspace (Docs/Sheets/Slides) não têm
//!   conteúdo binário próprio e não podem ser baixados via `alt=media`
//!   (exigem `files.export` para um formato de exportação) — não
//!   implementado nesta entrega; `open_download` deles falha com o erro
//!   que o próprio Google retorna, não é tratado como caso especial.
//! - **Nunca testado contra uma conta Google real** (sem projeto Google
//!   Cloud/credenciais disponíveis neste ambiente) — construído seguindo a
//!   documentação pública da API v3, mas, como aconteceu com o OneDrive
//!   (ver `NexoFS_TASKS_v1.0.md`, bugs reais só encontrados em validação ao
//!   vivo), é esperado que precise de ajustes ao validar contra o Google de
//!   verdade pela primeira vez.

mod config;
mod dto;
mod http_error;
mod mapping;

pub use config::GoogleDriveConfig;

use async_trait::async_trait;
use dto::{GoogleChangeList, GoogleFile, GoogleFileList, GoogleStartPageToken, GoogleUserInfo, TokenResponse, FOLDER_MIME_TYPE};
use futures_util::TryStreamExt;
use http_error::{ensure_success, map_transport_error};
use mapping::map_file;
use nexofs_domain::{ProviderId, RemoteItemId, SecretToken};
use nexofs_provider_api::*;
use reqwest::{Client, Method};
use std::collections::HashMap;
use url::Url;

const DRIVE_BASE: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
const FILE_FIELDS: &str = "id,name,mimeType,size,version,md5Checksum,parents,modifiedTime,createdTime,trashed";
/// Múltiplo de 256 KiB exigido pela Drive API para blocos intermediários de
/// upload resumível (documentação oficial); 8 MiB equilibra número de
/// requisições contra quanto se perde ao reenviar um bloco após falha.
const RESUMABLE_CHUNK_SIZE_BYTES: u64 = 8 * 1024 * 1024;

pub struct GoogleDriveProvider {
    http: Client,
    config: GoogleDriveConfig,
    /// `provider_account_id` (id do usuário Google) → id real da pasta raiz
    /// do "Meu Drive" dessa conta. Ao contrário do OneDrive (que cacheia por
    /// `namespace_remote_id`, globalmente único por drive), aqui múltiplas
    /// contas compartilham o mesmo `namespace_remote_id` literal (`"root"`,
    /// só um alias) — cachear por isso colidiria entre contas diferentes
    /// usando a mesma instância do provider.
    root_id_cache: tokio::sync::Mutex<HashMap<String, String>>,
}

impl GoogleDriveProvider {
    pub fn new(config: GoogleDriveConfig) -> Self {
        Self {
            http: Client::builder()
                .user_agent("NexoFS/0.1")
                .connect_timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("configuração de TLS/DNS do reqwest é estática e válida"),
            config,
            root_id_cache: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn resolve_root_id(&self, account: &ProviderAccountContext) -> ProviderResult<String> {
        if let Some(id) = self.root_id_cache.lock().await.get(&account.provider_account_id) {
            return Ok(id.clone());
        }
        let root: GoogleFile = self.get_json(&format!("{DRIVE_BASE}/files/root?fields=id"), &account.access_token).await?;
        self.root_id_cache.lock().await.insert(account.provider_account_id.clone(), root.id.clone());
        Ok(root.id)
    }

    /// Normaliza `parent_remote_item_id` para `None` quando aponta para a
    /// raiz do "Meu Drive" desta conta — mesma tradução que o adaptador
    /// OneDrive faz para a convenção neutra "sem pai" = `None` (SPEC §5.1).
    fn normalize_top_level(mut item: RemoteItem, root_id: &str) -> RemoteItem {
        if item.parent_remote_item_id.as_ref().map(|id| id.as_ref()) == Some(root_id) {
            item.parent_remote_item_id = None;
        }
        item
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str, token: &SecretToken) -> ProviderResult<T> {
        let response = self.http.get(url).bearer_auth(token.expose()).send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        response.json::<T>().await.map_err(map_transport_error)
    }

    fn escape_query_literal(value: &str) -> String {
        value.replace('\\', "\\\\").replace('\'', "\\'")
    }

    /// Drive permite nomes duplicados numa pasta (diferente do Graph) — este
    /// é o único jeito de saber se "já existe algo com este nome" antes de
    /// decidir criar vs. atualizar/recusar.
    async fn find_child_by_name(&self, parent: Option<&RemoteItemId>, name: &str, token: &SecretToken) -> ProviderResult<Option<GoogleFile>> {
        let parent_id = parent.map(|p| p.as_ref().to_string()).unwrap_or_else(|| "root".to_string());
        let query = format!(
            "'{}' in parents and trashed = false and name = '{}'",
            Self::escape_query_literal(&parent_id),
            Self::escape_query_literal(name)
        );
        let mut url = Url::parse(&format!("{DRIVE_BASE}/files")).expect("URL base estática válida");
        url.query_pairs_mut().append_pair("q", &query).append_pair("fields", &format!("files({FILE_FIELDS})")).append_pair("pageSize", "1");
        let page: GoogleFileList = self.get_json(url.as_str(), token).await?;
        Ok(page.files.into_iter().next())
    }

    /// Drive não documenta um cabeçalho de precondition HTTP para escrita
    /// condicional (ao contrário do `If-Match` do Graph) — esta é uma
    /// checagem "ler-depois-escrever" no cliente: mais fraca que um
    /// precondition atômico no servidor (uma corrida estreita entre o GET e
    /// a escrita seguinte tecnicamente ainda é possível), mas ainda detecta
    /// a esmagadora maioria das edições concorrentes que T3-07/FR-UP-006
    /// existem para pegar, em vez de nunca checar nada.
    async fn check_version_precondition(&self, remote_item_id: &str, base_version: &str, token: &SecretToken) -> ProviderResult<()> {
        let current: GoogleFile = self.get_json(&format!("{DRIVE_BASE}/files/{remote_item_id}?fields=version"), token).await?;
        match current.version {
            Some(v) if v == base_version => Ok(()),
            _ => Err(ProviderError::new(ProviderErrorKind::VersionConflict, "a versão remota mudou desde a última sincronização conhecida deste item")),
        }
    }

    async fn create_upload_session(
        &self,
        parent: Option<&RemoteItemId>,
        name: &str,
        existing_file_id: Option<&str>,
        token: &SecretToken,
    ) -> ProviderResult<String> {
        let mut body = serde_json::json!({ "name": name });
        let (method, url) = match existing_file_id {
            Some(id) => (Method::PATCH, format!("{UPLOAD_BASE}/files/{id}?uploadType=resumable")),
            None => {
                if let Some(parent) = parent {
                    body["parents"] = serde_json::json!([parent.as_ref()]);
                }
                (Method::POST, format!("{UPLOAD_BASE}/files?uploadType=resumable"))
            }
        };

        let response = self.http.request(method, &url).bearer_auth(token.expose()).json(&body).send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::CorruptResponse, "Drive não retornou Location para a sessão de upload resumível"))
    }

    /// Envia o conteúdo em blocos para a `uploadUrl` da sessão — mesma
    /// estrutura de `OneDriveProvider::upload_resumable`. Retomada de
    /// progresso parcial após queda fica para o dispatcher de recuperação
    /// (T3-10/T6-03), igual ao OneDrive: a sessão sempre é consumida do
    /// byte 0 aqui, sempre correto/seguro, só não o mais eficiente possível
    /// após uma interrupção a meio de um arquivo grande.
    async fn upload_via_session(&self, upload_url: &str, mut content: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>, total: u64) -> ProviderResult<GoogleFile> {
        let mut offset: u64 = 0;
        if total == 0 {
            // Bloco vazio ainda precisa de uma chamada para "fechar" a
            // sessão — Content-Range com total 0 é o formato que a Drive
            // API documenta para este caso.
            let response = self
                .http
                .put(upload_url)
                .header(reqwest::header::CONTENT_RANGE, "bytes */0")
                .header(reqwest::header::CONTENT_LENGTH, 0)
                .send()
                .await
                .map_err(map_transport_error)?;
            let response = ensure_success(response).await?;
            return response.json().await.map_err(map_transport_error);
        }

        loop {
            let chunk_len = RESUMABLE_CHUNK_SIZE_BYTES.min(total - offset);
            let mut buf = vec![0u8; chunk_len as usize];
            tokio::io::AsyncReadExt::read_exact(&mut content, &mut buf).await.map_err(|e| ProviderError::new(ProviderErrorKind::Network, e.to_string()))?;

            let response = self
                .http
                .put(upload_url)
                .header(reqwest::header::CONTENT_RANGE, format!("bytes {}-{}/{}", offset, offset + chunk_len - 1, total))
                .header(reqwest::header::CONTENT_LENGTH, chunk_len)
                .body(buf)
                .send()
                .await
                .map_err(map_transport_error)?;
            let response = ensure_success(response).await?;
            offset += chunk_len;

            if offset >= total {
                return response.json().await.map_err(map_transport_error);
            }
            // 308 Resume Incomplete intermediário, sem corpo relevante.
        }
    }
}

#[async_trait]
impl CloudProvider for GoogleDriveProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::from("googledrive"),
            display_name: "Google Drive".to_string(),
            capabilities: ProviderCapabilities {
                incremental_changes: true,
                latest_cursor_without_full_scan: true,
                push_notifications: false,
                metadata_batch: false,
                resumable_upload: true,
                ranged_download: true,
                stable_item_ids: true,
                content_version: true,
                metadata_version: true,
                remote_hashes: vec![HashAlgorithm::Md5],
                // Mover exige 2 chamadas (ver `move_item`) — não atômico
                // como a `PATCH` única do Graph.
                atomic_move: false,
                server_side_copy: true,
                trash: true,
                // Drive trata nomes como texto opaco sensível a maiúsculas E
                // permite duplicados — não há normalização de "preservar
                // caixa, comparar sem diferenciar" como o OneDrive faz.
                case_sensitivity: CaseSensitivity::Sensitive,
                max_simple_upload_bytes: None,
                max_item_name_bytes: Some(32760),
                max_path_bytes: None,
            },
        }
    }

    fn build_authorization_url(&self, redirect_uri: &str, state: &str, pkce_challenge: &str) -> String {
        let mut url = Url::parse(self.config.authorize_endpoint()).expect("authorize_endpoint sempre produz uma URL válida");
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &self.config.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("code_challenge", pkce_challenge)
            .append_pair("code_challenge_method", "S256")
            // `access_type=offline` pede um refresh_token; `prompt=consent`
            // garante que ele venha mesmo se o usuário já tiver consentido
            // antes nesta instalação (o Google só devolve refresh_token na
            // primeira vez por padrão, o que quebraria "adicionar a mesma
            // conta de novo depois de excluí-la").
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");
        url.to_string()
    }

    async fn authenticate(&self, request: AuthenticationRequest) -> ProviderResult<AuthenticatedAccount> {
        if !self.config.is_configured() {
            return Err(ProviderError::new(
                ProviderErrorKind::Permanent,
                "Google Drive não está configurado — defina NEXOFS_GOOGLEDRIVE_CLIENT_ID e NEXOFS_GOOGLEDRIVE_CLIENT_SECRET (veja a documentação para registrar um projeto no Google Cloud Console)",
            ));
        }
        let params = [
            ("client_id", self.config.client_id.as_str()),
            ("client_secret", self.config.client_secret.as_str()),
            ("grant_type", "authorization_code"),
            ("code", request.authorization_code.expose()),
            ("redirect_uri", request.redirect_uri.as_str()),
            ("code_verifier", request.pkce_verifier.expose()),
        ];

        let response = self.http.post(self.config.token_endpoint()).form(&params).send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let token: TokenResponse = response.json().await.map_err(map_transport_error)?;

        let refresh_token = token.refresh_token.ok_or_else(|| {
            ProviderError::new(ProviderErrorKind::Permanent, "resposta de token sem refresh_token — verifique access_type=offline/prompt=consent")
        })?;

        let access_token = SecretToken::new(token.access_token);
        let userinfo: GoogleUserInfo = self.get_json(USERINFO_URL, &access_token).await?;

        Ok(AuthenticatedAccount {
            provider_account_id: userinfo.id,
            display_name: userinfo.name.or(userinfo.email).unwrap_or_else(|| "Conta Google Drive".to_string()),
            tenant_id: None,
            access_token,
            access_token_expires_at_unix: chrono::Utc::now().timestamp() + token.expires_in,
            refresh_token: SecretToken::new(refresh_token),
        })
    }

    async fn refresh_authentication(&self, account: &ProviderAccountContext) -> ProviderResult<AuthenticationState> {
        let result = self.get_json::<GoogleUserInfo>(USERINFO_URL, &account.access_token).await;
        match result {
            Ok(_) => Ok(AuthenticationState::Valid { expires_at_unix: chrono::Utc::now().timestamp() + 300 }),
            Err(e) if e.kind == ProviderErrorKind::AuthenticationRequired => Ok(AuthenticationState::RequiresReauthentication),
            Err(e) => Err(e),
        }
    }

    async fn refresh_via_refresh_token(&self, refresh_token: &SecretToken) -> ProviderResult<AuthenticatedAccount> {
        let params = [
            ("client_id", self.config.client_id.as_str()),
            ("client_secret", self.config.client_secret.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose()),
        ];
        let response = self.http.post(self.config.token_endpoint()).form(&params).send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let token: TokenResponse = response.json().await.map_err(map_transport_error)?;

        // O Google só reenvia um refresh_token novo quando ele muda (raro);
        // quando ausente, o original continua válido e deve ser preservado.
        let refresh_token = token.refresh_token.map(SecretToken::new).unwrap_or_else(|| refresh_token.clone());
        let access_token = SecretToken::new(token.access_token);
        let userinfo: GoogleUserInfo = self.get_json(USERINFO_URL, &access_token).await?;

        Ok(AuthenticatedAccount {
            provider_account_id: userinfo.id,
            display_name: userinfo.name.or(userinfo.email).unwrap_or_else(|| "Conta Google Drive".to_string()),
            tenant_id: None,
            access_token,
            access_token_expires_at_unix: chrono::Utc::now().timestamp() + token.expires_in,
            refresh_token,
        })
    }

    async fn list_namespaces(&self, _account: &ProviderAccountContext) -> ProviderResult<Vec<RemoteNamespace>> {
        // "Meu Drive" é sempre o único namespace por conta pessoal — Drives
        // Compartilhados (Shared Drives, antigo Team Drives) ficam de fora
        // desta entrega, mesmo escopo de "SharePoint fica para depois" já
        // decidido para o OneDrive.
        Ok(vec![RemoteNamespace { remote_namespace_id: "root".to_string(), display_name: "Google Drive".to_string(), kind: NamespaceKind::Personal }])
    }

    async fn list_children(&self, request: ListChildrenRequest) -> ProviderResult<RemotePage<RemoteItem>> {
        let root_id = self.resolve_root_id(&request.account).await?;
        let parent_id = request.parent_remote_item_id.as_ref().map(|id| id.as_ref().to_string()).unwrap_or_else(|| "root".to_string());
        let query = format!("'{}' in parents and trashed = false", Self::escape_query_literal(&parent_id));

        let mut url = Url::parse(&format!("{DRIVE_BASE}/files")).expect("URL base estática válida");
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("q", &query).append_pair("fields", &format!("nextPageToken,files({FILE_FIELDS})")).append_pair("pageSize", "1000");
            if let Some(token) = &request.page_token {
                qp.append_pair("pageToken", token);
            }
        }

        let page: GoogleFileList = self.get_json(url.as_str(), &request.account.access_token).await?;
        Ok(RemotePage {
            items: page.files.into_iter().map(map_file).map(|item| Self::normalize_top_level(item, &root_id)).collect(),
            next_page_token: page.next_page_token,
        })
    }

    async fn get_item(&self, request: GetItemRequest) -> ProviderResult<Option<RemoteItem>> {
        let url = format!("{DRIVE_BASE}/files/{}?fields={FILE_FIELDS}", request.remote_item_id.as_ref());
        match self.get_json::<GoogleFile>(&url, &request.account.access_token).await {
            Ok(item) => {
                let root_id = self.resolve_root_id(&request.account).await?;
                Ok(Some(Self::normalize_top_level(map_file(item), &root_id)))
            }
            Err(e) if e.kind == ProviderErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn create_change_cursor(&self, request: CreateCursorRequest) -> ProviderResult<ChangeCursor> {
        // A Changes API do Drive só tem um modo — "a partir de agora"; não
        // existe um equivalente ao `/delta` do Graph sem token para
        // enumerar todo o histórico. `latest_only` portanto não muda o
        // comportamento aqui (na prática, `nexofs-sync-core` só chama isto
        // com `true` de qualquer forma — a indexação preguiçosa via
        // `list_children` nunca precisou do outro modo).
        let _ = request.latest_only;
        let page: GoogleStartPageToken = self.get_json(&format!("{DRIVE_BASE}/changes/startPageToken"), &request.account.access_token).await?;
        Ok(ChangeCursor(page.start_page_token))
    }

    async fn list_changes(&self, request: ListChangesRequest) -> ProviderResult<ChangePage> {
        let mut url = Url::parse(&format!("{DRIVE_BASE}/changes")).expect("URL base estática válida");
        url.query_pairs_mut()
            .append_pair("pageToken", &request.cursor.0)
            .append_pair("pageSize", "1000")
            .append_pair("fields", &format!("nextPageToken,newStartPageToken,changes(fileId,removed,file({FILE_FIELDS}))"));

        let page: GoogleChangeList = self.get_json(url.as_str(), &request.account.access_token).await?;
        let root_id = self.resolve_root_id(&request.account).await?;

        let changes = page
            .changes
            .into_iter()
            .filter_map(|change| {
                if change.removed {
                    return Some(RemoteChange::Deleted { remote_item_id: RemoteItemId::from(change.file_id) });
                }
                change.file.map(|file| {
                    // Mover para a lixeira não passa por `removed` — só o
                    // facet `trashed` do próprio arquivo muda; sem checar
                    // isso, um item na lixeira ficaria indexado como
                    // presente para sempre.
                    if file.trashed {
                        RemoteChange::Deleted { remote_item_id: RemoteItemId::from(change.file_id.clone()) }
                    } else {
                        RemoteChange::Upserted(Self::normalize_top_level(map_file(file), &root_id))
                    }
                })
            })
            .collect();

        let has_more = page.next_page_token.is_some();
        let next_cursor = page
            .next_page_token
            .or(page.new_start_page_token)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::CorruptResponse, "página de changes sem nextPageToken nem newStartPageToken"))?;

        Ok(ChangePage { changes, next_cursor: ChangeCursor(next_cursor), has_more })
    }

    async fn open_download(&self, request: DownloadRequest) -> ProviderResult<DownloadHandle> {
        let url = format!("{DRIVE_BASE}/files/{}?alt=media", request.remote_item_id.as_ref());
        let mut builder = self.http.get(&url).bearer_auth(request.account.access_token.expose());
        if let Some(range) = request.range {
            builder = builder.header(reqwest::header::RANGE, format!("bytes={}-{}", range.start, range.end));
        }
        let response = builder.send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;

        let content_length = response.content_length();
        let stream = response.bytes_stream().map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
        let reader = tokio_util::io::StreamReader::new(stream);

        Ok(DownloadHandle { reader: Box::pin(reader), content_length, remote_content_version: None })
    }

    async fn upload(&self, request: UploadRequest) -> ProviderResult<UploadResult> {
        // Desestruturar antes de qualquer `.await` evita reter uma
        // referência a `UploadRequest` através do ponto de suspensão (mesmo
        // motivo documentado em `OneDriveProvider::upload_resumable`).
        let UploadRequest { account, namespace_remote_id: _, parent_remote_item_id, name, size_bytes, base_remote_version, content, resumable_session_token } =
            request;

        let existing = if resumable_session_token.is_none() {
            self.find_child_by_name(parent_remote_item_id.as_ref(), &name, &account.access_token).await?
        } else {
            None
        };

        if let (Some(existing), Some(base_version)) = (&existing, &base_remote_version) {
            self.check_version_precondition(&existing.id, base_version, &account.access_token).await?;
        }

        let upload_url = match resumable_session_token {
            Some(token) => token,
            None => self.create_upload_session(parent_remote_item_id.as_ref(), &name, existing.as_ref().map(|f| f.id.as_str()), &account.access_token).await?,
        };

        let item = self.upload_via_session(&upload_url, content, size_bytes).await?;
        Ok(UploadResult { item: map_file(item), resumable_session_token: None })
    }

    async fn create_directory(&self, request: CreateDirectoryRequest) -> ProviderResult<RemoteItem> {
        // Drive não tem um "falhar se já existir" nativo (permite nomes
        // duplicados) — replicamos essa semântica no cliente, mesmo
        // contrato que `POST .../children` com `conflictBehavior: fail` dá
        // de graça no Graph.
        if self.find_child_by_name(request.parent_remote_item_id.as_ref(), &request.name, &request.account.access_token).await?.is_some() {
            return Err(ProviderError::new(ProviderErrorKind::AlreadyExists, format!("já existe um item chamado '{}' nesta pasta", request.name)));
        }

        let mut body = serde_json::json!({ "name": request.name, "mimeType": FOLDER_MIME_TYPE });
        if let Some(parent) = &request.parent_remote_item_id {
            body["parents"] = serde_json::json!([parent.as_ref()]);
        }

        let url = format!("{DRIVE_BASE}/files?fields={FILE_FIELDS}");
        let response = self.http.post(&url).bearer_auth(request.account.access_token.expose()).json(&body).send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let item: GoogleFile = response.json().await.map_err(map_transport_error)?;
        Ok(map_file(item))
    }

    async fn move_item(&self, request: MoveItemRequest) -> ProviderResult<RemoteItem> {
        if let Some(base_version) = &request.base_remote_version {
            self.check_version_precondition(request.remote_item_id.as_ref(), base_version, &request.account.access_token).await?;
        }

        let mut body = serde_json::Map::new();
        if let Some(new_name) = &request.new_name {
            body.insert("name".to_string(), serde_json::Value::String(new_name.clone()));
        }

        let mut url = Url::parse(&format!("{DRIVE_BASE}/files/{}", request.remote_item_id.as_ref())).expect("URL base estática válida");
        url.query_pairs_mut().append_pair("fields", FILE_FIELDS);

        // A Drive API exige `addParents`/`removeParents` explícitos —
        // diferente do Graph, que só precisa do novo `parentReference.id`
        // numa única `PATCH`. Por isso descobrimos o pai atual primeiro:
        // sem `removeParents`, o item ficaria com DOIS pais (aparecendo em
        // duas pastas ao mesmo tempo), quebrando o modelo de árvore única
        // que o resto do NexoFS assume.
        //
        // Bug real de produção: isto só rodava quando `new_parent_remote_item_id`
        // era `Some(..)` — mover de volta pra raiz (`None`, por convenção:
        // a raiz sintética não tem `remote_item_id`) pulava o bloco inteiro,
        // sem `addParents` nem `removeParents`, um no-op silencioso no
        // Drive. `"root"` é o alias documentado da Drive API pra pasta raiz
        // do usuário, então serve como `new_parent` mesmo sem um id real.
        let current: GoogleFile = self.get_json(&format!("{DRIVE_BASE}/files/{}?fields=parents", request.remote_item_id.as_ref()), &request.account.access_token).await?;
        let old_parent = current.parents.and_then(|mut parents| if parents.is_empty() { None } else { Some(parents.remove(0)) });
        let new_parent: &str = match &request.new_parent_remote_item_id {
            Some(id) => id.as_ref(),
            None => "root",
        };
        url.query_pairs_mut().append_pair("addParents", new_parent);
        if let Some(old_parent) = &old_parent {
            if old_parent != new_parent {
                url.query_pairs_mut().append_pair("removeParents", old_parent);
            }
        }

        let response = self.http.patch(url.as_str()).bearer_auth(request.account.access_token.expose()).json(&body).send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let item: GoogleFile = response.json().await.map_err(map_transport_error)?;
        Ok(map_file(item))
    }

    async fn delete_item(&self, request: DeleteItemRequest) -> ProviderResult<()> {
        if let Some(base_version) = &request.base_remote_version {
            self.check_version_precondition(request.remote_item_id.as_ref(), base_version, &request.account.access_token).await?;
        }
        // "Excluir" no Drive, como no OneDrive (`capabilities.trash =
        // true`), move para a lixeira em vez de apagar de vez — reversível
        // via `restore_item`.
        let url = format!("{DRIVE_BASE}/files/{}", request.remote_item_id.as_ref());
        let response = self
            .http
            .patch(&url)
            .bearer_auth(request.account.access_token.expose())
            .json(&serde_json::json!({ "trashed": true }))
            .send()
            .await
            .map_err(map_transport_error)?;
        ensure_success(response).await?;
        Ok(())
    }

    async fn restore_item(&self, request: RestoreItemRequest) -> ProviderResult<RemoteItem> {
        let url = format!("{DRIVE_BASE}/files/{}?fields={FILE_FIELDS}", request.remote_item_id.as_ref());
        let response = self
            .http
            .patch(&url)
            .bearer_auth(request.account.access_token.expose())
            .json(&serde_json::json!({ "trashed": false }))
            .send()
            .await
            .map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let item: GoogleFile = response.json().await.map_err(map_transport_error)?;
        Ok(map_file(item))
    }
}
