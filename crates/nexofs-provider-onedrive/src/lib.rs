//! Adaptador Microsoft OneDrive (pessoal e corporativo). SPEC §6.
//!
//! Único crate do workspace autorizado a depender de conceitos do
//! Microsoft Graph (FR-MC-001) — todo tipo que sai daqui já está convertido
//! para o modelo neutro de `nexofs-provider-api`.

mod config;
mod dto;
mod graph_error;
mod mapping;

pub use config::{OneDriveConfig, TenantHint};

use async_trait::async_trait;
use dto::{GraphChildrenPage, GraphDeltaPage, GraphDrive, GraphDriveItem, GraphUploadSession, GraphUser, TokenResponse};
use futures_util::TryStreamExt;
use graph_error::{ensure_success, map_transport_error};
use mapping::map_drive_item;
use nexofs_domain::{ProviderId, RemoteItemId, SecretToken};
use nexofs_provider_api::*;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::{Client, Method};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
/// Teto do upload simples do Graph (SPEC §16.4 / `max_simple_upload_bytes`).
/// Acima disso é obrigatório usar sessão de upload resumível.
const SIMPLE_UPLOAD_LIMIT_BYTES: u64 = 4 * 1024 * 1024;
/// Tamanho de bloco para upload resumível — DEVE ser múltiplo de 320 KiB
/// (exigência do Graph); 10 MiB equilibra número de requisições contra
/// quanto se perde ao reenviar um bloco após uma falha de rede.
const RESUMABLE_CHUNK_SIZE_BYTES: u64 = 10 * 1024 * 1024;

pub struct OneDriveProvider {
    http: Client,
    config: OneDriveConfig,
    /// `namespace_remote_id` (drive) → id real do objeto-raiz do drive no
    /// Graph. Todo `parentReference.id` de um item de nível superior aponta
    /// para este id (nunca vem `null` — só o próprio objeto-raiz não tem
    /// pai); sem normalizar isso para o `None` que o resto do NexoFS espera
    /// como "sem pai" (SPEC §5.1), todo item de nível superior seria
    /// reparentado para uma pasta fantasma a cada sincronização incremental
    /// (bug real encontrado validando a Fase 3 — ver `dto::GraphDriveItem::root`).
    root_id_cache: tokio::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl OneDriveProvider {
    pub fn new(config: OneDriveConfig) -> Self {
        Self {
            http: Client::builder()
                .user_agent("NexoFS/0.1")
                // Protege contra uma conexão que nunca se estabelece (rede
                // instável, servidor não responde ao handshake). Não usamos
                // `.timeout()` total: arquivos podem ter até 100 GB
                // (PRD §15.2) e um timeout fim-a-fim abortaria downloads
                // legítimos de longa duração. Um timeout de inatividade
                // (sem progresso de bytes) fica para a Fase 3 junto do
                // upload/download resumível (T3-06).
                .connect_timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("configuração de TLS/DNS do reqwest é estática e válida"),
            config,
            root_id_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Resolve (e cacheia) o id real do objeto-raiz do drive — estável pela
    /// vida do drive, então uma única chamada de rede por drive já basta
    /// para o processo inteiro.
    async fn resolve_root_id(&self, drive: &str, token: &SecretToken) -> ProviderResult<String> {
        if let Some(id) = self.root_id_cache.lock().await.get(drive) {
            return Ok(id.clone());
        }
        let root: GraphDriveItem = self.get_json(&format!("{GRAPH_BASE}/drives/{drive}/root"), token).await?;
        self.root_id_cache.lock().await.insert(drive.to_string(), root.id.clone());
        Ok(root.id)
    }

    /// Normaliza `parent_remote_item_id` para `None` quando ele aponta para
    /// o objeto-raiz do drive — traduz a convenção do Graph ("todo item de
    /// nível superior tem `parentReference.id` = id da raiz") para a
    /// convenção neutra do NexoFS ("sem pai" = `None`, SPEC §5.1).
    fn normalize_top_level(mut item: RemoteItem, root_id: &str) -> RemoteItem {
        if item.parent_remote_item_id.as_ref().map(|id| id.as_ref()) == Some(root_id) {
            item.parent_remote_item_id = None;
        }
        item
    }

    fn encode_name(name: &str) -> String {
        utf8_percent_encode(name, NON_ALPHANUMERIC).to_string()
    }

    fn children_url(&self, drive: &str, parent: Option<&str>) -> String {
        match parent {
            Some(id) => format!("{GRAPH_BASE}/drives/{drive}/items/{id}/children"),
            None => format!("{GRAPH_BASE}/drives/{drive}/root/children"),
        }
    }

    fn item_url(&self, drive: &str, item_id: &str) -> String {
        format!("{GRAPH_BASE}/drives/{drive}/items/{item_id}")
    }

    fn content_by_id_url(&self, drive: &str, item_id: &str) -> String {
        format!("{GRAPH_BASE}/drives/{drive}/items/{item_id}/content")
    }

    fn content_by_name_url(&self, drive: &str, parent: Option<&str>, name: &str) -> String {
        let encoded = Self::encode_name(name);
        match parent {
            Some(id) => format!("{GRAPH_BASE}/drives/{drive}/items/{id}:/{encoded}:/content"),
            None => format!("{GRAPH_BASE}/drives/{drive}/root:/{encoded}:/content"),
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        token: &SecretToken,
    ) -> ProviderResult<T> {
        let response = self
            .http
            .get(url)
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        response.json::<T>().await.map_err(map_transport_error)
    }

    /// Busca um filho por nome dentro do pai — usado para decidir entre
    /// criar (`PUT .../root:/{name}:/content`) ou atualizar por id
    /// (`PUT .../items/{id}/content`) em `upload`.
    async fn find_child_by_name(
        &self,
        drive: &str,
        parent: Option<&RemoteItemId>,
        name: &str,
        token: &SecretToken,
    ) -> ProviderResult<Option<GraphDriveItem>> {
        let url = self.children_url(drive, parent.map(|p| p.as_ref()));
        let page: GraphChildrenPage = self.get_json(&url, token).await?;
        Ok(page.value.into_iter().find(|item| item.name.as_deref() == Some(name)))
    }

    /// Abre uma sessão de upload resumível (SPEC §6.1/§16.4). `conflictBehavior`
    /// segue a mesma convenção de `create_directory`: `fail` para um item
    /// que nunca existiu no remoto (`base_remote_version` ausente — criação
    /// pura), `replace` para atualizar um item já sincronizado.
    #[allow(clippy::too_many_arguments)]
    async fn create_upload_session(
        &self,
        namespace_remote_id: &str,
        parent_remote_item_id: Option<&RemoteItemId>,
        name: &str,
        base_remote_version: &Option<String>,
        token: &SecretToken,
    ) -> ProviderResult<String> {
        let encoded_name = Self::encode_name(name);
        let url = match parent_remote_item_id {
            Some(parent) => format!(
                "{GRAPH_BASE}/drives/{namespace_remote_id}/items/{}:/{encoded_name}:/createUploadSession",
                parent.as_ref()
            ),
            None => format!("{GRAPH_BASE}/drives/{namespace_remote_id}/root:/{encoded_name}:/createUploadSession"),
        };
        let conflict_behavior = if base_remote_version.is_some() { "replace" } else { "fail" };
        let body = serde_json::json!({
            "item": { "@microsoft.graph.conflictBehavior": conflict_behavior },
        });

        let response = self
            .http
            .post(&url)
            .bearer_auth(token.expose())
            .json(&body)
            .send()
            .await
            .map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let session: GraphUploadSession = response.json().await.map_err(map_transport_error)?;
        Ok(session.upload_url)
    }

    /// Envia o conteúdo em blocos de `RESUMABLE_CHUNK_SIZE_BYTES` para a
    /// `uploadUrl` da sessão (SPEC §16.4). Importante: as requisições de
    /// bloco NÃO levam `Authorization` — a `uploadUrl` já é uma URL
    /// pré-autorizada (SAS-like) específica desta sessão; anexar o bearer
    /// token não é necessário e a documentação do Graph recomenda contra.
    ///
    /// Retomada de progresso parcial após queda (persistir `bytes_sent` em
    /// `payload_json` e recomeçar dali, em vez do zero) fica para o
    /// dispatcher de recuperação da Fase 3 (T3-10) — aqui a sessão sempre é
    /// consumida do byte 0, o que é sempre correto/seguro, só não é o mais
    /// eficiente possível após uma interrupção a meio de um arquivo grande.
    async fn upload_resumable(&self, request: UploadRequest) -> ProviderResult<UploadResult> {
        // Desestruturar antes de qualquer `.await` evita reter uma
        // referência a `UploadRequest` (que contém `content: Pin<Box<dyn
        // AsyncRead + Send>>`, não `Sync`) através do ponto de suspensão —
        // isso quebraria o `Send` exigido pelo `#[async_trait]` no retorno
        // do trait `CloudProvider`.
        let UploadRequest {
            account,
            namespace_remote_id,
            parent_remote_item_id,
            name,
            size_bytes,
            base_remote_version,
            mut content,
            resumable_session_token,
        } = request;

        let upload_url = match resumable_session_token {
            Some(token) => token,
            None => {
                self.create_upload_session(&namespace_remote_id, parent_remote_item_id.as_ref(), &name, &base_remote_version, &account.access_token)
                    .await?
            }
        };

        let total = size_bytes;
        let mut offset: u64 = 0;

        loop {
            let chunk_len = RESUMABLE_CHUNK_SIZE_BYTES.min(total - offset);
            let mut buf = vec![0u8; chunk_len as usize];
            tokio::io::AsyncReadExt::read_exact(&mut content, &mut buf)
                .await
                .map_err(|e| ProviderError::new(ProviderErrorKind::Network, e.to_string()))?;

            let response = self
                .http
                .put(&upload_url)
                .header(reqwest::header::CONTENT_RANGE, format!("bytes {}-{}/{}", offset, offset + chunk_len - 1, total))
                .header(reqwest::header::CONTENT_LENGTH, chunk_len)
                .body(buf)
                .send()
                .await
                .map_err(map_transport_error)?;
            let response = ensure_success(response).await?;
            offset += chunk_len;

            if offset >= total {
                let item: GraphDriveItem = response.json().await.map_err(map_transport_error)?;
                return Ok(UploadResult {
                    item: map_drive_item(item),
                    resumable_session_token: None,
                });
            }
            // Resposta intermediária (202, sem DriveItem) — só o bloco final
            // retorna o item criado/atualizado.
        }
    }
}

#[async_trait]
impl CloudProvider for OneDriveProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::from("onedrive"),
            display_name: "Microsoft OneDrive".to_string(),
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
                remote_hashes: vec![HashAlgorithm::QuickXorHash, HashAlgorithm::Sha1],
                atomic_move: true,
                server_side_copy: false,
                trash: true,
                case_sensitivity: CaseSensitivity::InsensitivePreserving,
                max_simple_upload_bytes: Some(SIMPLE_UPLOAD_LIMIT_BYTES),
                max_item_name_bytes: Some(400),
                max_path_bytes: Some(400),
            },
        }
    }

    fn build_authorization_url(&self, redirect_uri: &str, state: &str, pkce_challenge: &str) -> String {
        let scope = self.config.scopes.join(" ");
        let mut url = url::Url::parse(&self.config.authorize_endpoint())
            .expect("authorize_endpoint sempre produz uma URL válida");
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_mode", "query")
            .append_pair("scope", &scope)
            .append_pair("state", state)
            .append_pair("code_challenge", pkce_challenge)
            .append_pair("code_challenge_method", "S256");
        url.to_string()
    }

    async fn authenticate(
        &self,
        request: AuthenticationRequest,
    ) -> ProviderResult<AuthenticatedAccount> {
        let params = [
            ("client_id", self.config.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", request.authorization_code.expose()),
            ("redirect_uri", request.redirect_uri.as_str()),
            ("code_verifier", request.pkce_verifier.expose()),
            ("scope", &self.config.scopes.join(" ")),
        ];

        let response = self
            .http
            .post(self.config.token_endpoint())
            .form(&params)
            .send()
            .await
            .map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let token: TokenResponse = response.json().await.map_err(map_transport_error)?;

        let refresh_token = token.refresh_token.ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Permanent,
                "resposta de token sem refresh_token — verifique se o escopo offline_access foi concedido",
            )
        })?;

        let access_token = SecretToken::new(token.access_token);
        let me: GraphUser = self
            .get_json(&format!("{GRAPH_BASE}/me"), &access_token)
            .await?;

        Ok(AuthenticatedAccount {
            provider_account_id: me.id,
            display_name: me.display_name.unwrap_or_else(|| "Conta OneDrive".to_string()),
            tenant_id: match &self.config.tenant {
                TenantHint::Specific(id) => Some(id.clone()),
                TenantHint::Common => None,
            },
            access_token,
            access_token_expires_at_unix: chrono::Utc::now().timestamp() + token.expires_in,
            refresh_token: SecretToken::new(refresh_token),
        })
    }

    async fn refresh_authentication(
        &self,
        account: &ProviderAccountContext,
    ) -> ProviderResult<AuthenticationState> {
        // O token de acesso atual em `account` já reflete o último refresh
        // bem-sucedido feito por quem chama (o refresh token em si vive no
        // keyring, fora do alcance do adaptador — SPEC §22.2). Este método
        // apenas confirma que o access_token corrente ainda é válido.
        let result = self
            .get_json::<GraphUser>(&format!("{GRAPH_BASE}/me"), &account.access_token)
            .await;

        match result {
            Ok(_) => Ok(AuthenticationState::Valid {
                expires_at_unix: chrono::Utc::now().timestamp() + 300,
            }),
            Err(e) if e.kind == ProviderErrorKind::AuthenticationRequired => {
                Ok(AuthenticationState::RequiresReauthentication)
            }
            Err(e) => Err(e),
        }
    }

    /// T7-02: fluxo `grant_type=refresh_token` — usado pelo daemon para
    /// evitar exigir login interativo a cada reinício (NFR-REL do PRD
    /// "operações pendentes retomadas sem intervenção manual, salvo
    /// autenticação").
    async fn refresh_via_refresh_token(&self, refresh_token: &SecretToken) -> ProviderResult<AuthenticatedAccount> {
        let params = [
            ("client_id", self.config.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose()),
            ("scope", &self.config.scopes.join(" ")),
        ];

        let response = self
            .http
            .post(self.config.token_endpoint())
            .form(&params)
            .send()
            .await
            .map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let token: TokenResponse = response.json().await.map_err(map_transport_error)?;

        // A Microsoft pode rotacionar o refresh token a cada troca; quando
        // não retornado, o token original continua válido e deve ser
        // preservado pelo chamador.
        let refresh_token = token
            .refresh_token
            .map(SecretToken::new)
            .unwrap_or_else(|| refresh_token.clone());

        let access_token = SecretToken::new(token.access_token);
        let me: GraphUser = self
            .get_json(&format!("{GRAPH_BASE}/me"), &access_token)
            .await?;

        Ok(AuthenticatedAccount {
            provider_account_id: me.id,
            display_name: me.display_name.unwrap_or_else(|| "Conta OneDrive".to_string()),
            tenant_id: match &self.config.tenant {
                TenantHint::Specific(id) => Some(id.clone()),
                TenantHint::Common => None,
            },
            access_token,
            access_token_expires_at_unix: chrono::Utc::now().timestamp() + token.expires_in,
            refresh_token,
        })
    }

    async fn list_namespaces(
        &self,
        account: &ProviderAccountContext,
    ) -> ProviderResult<Vec<RemoteNamespace>> {
        let drive: GraphDrive = self
            .get_json(&format!("{GRAPH_BASE}/me/drive"), &account.access_token)
            .await?;

        let kind = match drive.drive_type.as_str() {
            "business" | "documentLibrary" => NamespaceKind::Shared,
            _ => NamespaceKind::Personal,
        };

        Ok(vec![RemoteNamespace {
            remote_namespace_id: drive.id,
            display_name: "OneDrive".to_string(),
            kind,
        }])
    }

    async fn list_children(
        &self,
        request: ListChildrenRequest,
    ) -> ProviderResult<RemotePage<RemoteItem>> {
        let url = match &request.page_token {
            Some(next_link) => next_link.clone(),
            None => self.children_url(
                &request.namespace_remote_id,
                request.parent_remote_item_id.as_ref().map(|id| id.as_ref()),
            ),
        };

        let page: GraphChildrenPage = self.get_json(&url, &request.account.access_token).await?;
        let root_id = self.resolve_root_id(&request.namespace_remote_id, &request.account.access_token).await?;
        Ok(RemotePage {
            items: page
                .value
                .into_iter()
                // Mesma defesa de `list_changes`: o objeto-raiz do drive não
                // deveria aparecer como filho de nada, mas se aparecer
                // nunca deve ser indexado como um item comum.
                .filter(|item| item.id != root_id)
                .map(map_drive_item)
                .map(|item| Self::normalize_top_level(item, &root_id))
                .collect(),
            next_page_token: page.next_link,
        })
    }

    async fn get_item(&self, request: GetItemRequest) -> ProviderResult<Option<RemoteItem>> {
        let url = self.item_url(&request.namespace_remote_id, request.remote_item_id.as_ref());
        match self
            .get_json::<GraphDriveItem>(&url, &request.account.access_token)
            .await
        {
            Ok(item) => {
                let root_id = self.resolve_root_id(&request.namespace_remote_id, &request.account.access_token).await?;
                Ok(Some(Self::normalize_top_level(map_drive_item(item), &root_id)))
            }
            Err(e) if e.kind == ProviderErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn create_change_cursor(
        &self,
        request: CreateCursorRequest,
    ) -> ProviderResult<ChangeCursor> {
        if request.latest_only {
            // "a partir de agora" (FR-IDX-004): pede à Graph um deltaLink
            // sem enumerar o histórico existente.
            let url = format!(
                "{GRAPH_BASE}/drives/{}/root/delta(token='latest')",
                request.namespace_remote_id
            );
            let page: GraphDeltaPage = self.get_json(&url, &request.account.access_token).await?;
            let delta_link = page.delta_link.ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::CorruptResponse,
                    "Graph não retornou @odata.deltaLink para token='latest'",
                )
            })?;
            Ok(ChangeCursor(delta_link))
        } else {
            // Cursor "de início" — list_changes fará a enumeração completa
            // seguindo @odata.nextLink até alcançar o @odata.deltaLink.
            Ok(ChangeCursor(format!(
                "{GRAPH_BASE}/drives/{}/root/delta",
                request.namespace_remote_id
            )))
        }
    }

    async fn list_changes(&self, request: ListChangesRequest) -> ProviderResult<ChangePage> {
        let page: GraphDeltaPage = self
            .get_json(&request.cursor.0, &request.account.access_token)
            .await?;
        let root_id = self.resolve_root_id(&request.namespace_remote_id, &request.account.access_token).await?;

        let changes = page
            .value
            .into_iter()
            // O próprio objeto-raiz do drive vem incluído na página de
            // delta em pelo menos algumas circunstâncias (comportamento do
            // Graph, nem sempre no facet `root` — bug real encontrado
            // validando a Fase 3: filtrar só por `item.root.is_some()` não
            // pegava todos os casos) — nunca é conteúdo do usuário, só um
            // artefato da API. Comparar o id diretamente contra o id
            // resolvido da raiz é a forma robusta de identificá-lo,
            // independente de qual facet a resposta incluiu. Sem filtrar
            // isso, `apply_remote_change` o indexa como uma pasta filha
            // comum da nossa raiz sintética. Mesmo filtrando, todo item de
            // nível superior *de verdade* também reporta este objeto como
            // pai (nunca `null`) — daí a normalização abaixo, que traduz
            // isso para o `None` que o resto do NexoFS entende como "sem pai".
            .filter(|item| item.root.is_none() && item.id != root_id)
            .map(|item| {
                if item.deleted.is_some() {
                    RemoteChange::Deleted {
                        remote_item_id: RemoteItemId::from(item.id),
                    }
                } else {
                    RemoteChange::Upserted(Self::normalize_top_level(map_drive_item(item), &root_id))
                }
            })
            .collect();

        let has_more = page.next_link.is_some();
        let next_cursor = page
            .next_link
            .or(page.delta_link)
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::CorruptResponse,
                    "página de delta sem nextLink nem deltaLink",
                )
            })?;

        Ok(ChangePage {
            changes,
            next_cursor: ChangeCursor(next_cursor),
            has_more,
        })
    }

    async fn open_download(&self, request: DownloadRequest) -> ProviderResult<DownloadHandle> {
        let url = self.content_by_id_url(&request.namespace_remote_id, request.remote_item_id.as_ref());
        let mut builder = self.http.get(&url).bearer_auth(request.account.access_token.expose());
        if let Some(range) = request.range {
            builder = builder.header(reqwest::header::RANGE, format!("bytes={}-{}", range.start, range.end));
        }
        let response = builder.send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;

        let content_length = response.content_length();
        let stream = response
            .bytes_stream()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
        let reader = tokio_util::io::StreamReader::new(stream);

        Ok(DownloadHandle {
            reader: Box::pin(reader),
            content_length,
            remote_content_version: None,
        })
    }

    async fn upload(&self, mut request: UploadRequest) -> ProviderResult<UploadResult> {
        // SPEC §6.1/§16.4: acima do teto de upload simples (ou quando já
        // existe uma sessão resumível em andamento vinda do journal), usa
        // sessão de upload por blocos — o Graph rejeita PUT direto acima de
        // 4 MiB.
        if request.resumable_session_token.is_some() || request.size_bytes > SIMPLE_UPLOAD_LIMIT_BYTES {
            return self.upload_resumable(request).await;
        }

        let mut bytes = Vec::with_capacity(request.size_bytes as usize);
        tokio::io::AsyncReadExt::read_to_end(&mut request.content, &mut bytes)
            .await
            .map_err(|e| ProviderError::new(ProviderErrorKind::Network, e.to_string()))?;

        let existing = self
            .find_child_by_name(
                &request.namespace_remote_id,
                request.parent_remote_item_id.as_ref(),
                &request.name,
                &request.account.access_token,
            )
            .await?;

        let url = match &existing {
            Some(item) => self.content_by_id_url(&request.namespace_remote_id, &item.id),
            None => self.content_by_name_url(
                &request.namespace_remote_id,
                request.parent_remote_item_id.as_ref().map(|id| id.as_ref()),
                &request.name,
            ),
        };

        // Bug real de produção: um arquivo de 0 bytes voltava "411 Length
        // Required" do Graph. `reqwest` não garante `Content-Length` para um
        // corpo vazio (a heurística interna decide entre isso e
        // `Transfer-Encoding: chunked`, que o endpoint de upload simples do
        // Graph não aceita) — declarar o header explicitamente remove a
        // ambiguidade para qualquer tamanho, não só zero.
        let content_length = bytes.len();
        let mut builder = self
            .http
            .put(&url)
            .bearer_auth(request.account.access_token.expose())
            .header(reqwest::header::CONTENT_LENGTH, content_length)
            .body(bytes);
        if let (Some(item), Some(base_version)) = (&existing, &request.base_remote_version) {
            let _ = item;
            builder = builder.header(reqwest::header::IF_MATCH, base_version.clone());
        }

        let response = builder.send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let item: GraphDriveItem = response.json().await.map_err(map_transport_error)?;

        Ok(UploadResult {
            item: map_drive_item(item),
            resumable_session_token: None,
        })
    }

    async fn create_directory(
        &self,
        request: CreateDirectoryRequest,
    ) -> ProviderResult<RemoteItem> {
        let url = self.children_url(
            &request.namespace_remote_id,
            request.parent_remote_item_id.as_ref().map(|id| id.as_ref()),
        );
        let body = serde_json::json!({
            "name": request.name,
            "folder": {},
            "@microsoft.graph.conflictBehavior": "fail",
        });

        let response = self
            .http
            .post(&url)
            .bearer_auth(request.account.access_token.expose())
            .json(&body)
            .send()
            .await
            .map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let item: GraphDriveItem = response.json().await.map_err(map_transport_error)?;
        Ok(map_drive_item(item))
    }

    async fn move_item(&self, request: MoveItemRequest) -> ProviderResult<RemoteItem> {
        let url = self.item_url(&request.namespace_remote_id, request.remote_item_id.as_ref());
        let mut body = serde_json::Map::new();
        // Bug real de produção: quando o destino é a raiz sintética,
        // `new_parent_remote_item_id` é `None` por convenção (a raiz não
        // tem `remote_item_id` — ver `resolve_remote_parent`). Omitir
        // `parentReference` do corpo do PATCH faz o Graph simplesmente não
        // tocar o pai atual (campos ausentes do corpo não são alterados) —
        // mover algo DE VOLTA para a raiz virava um no-op silencioso: a
        // operação completava (o Graph aceita o PATCH normalmente), o
        // índice local avançava, mas o item nunca saía de onde estava de
        // verdade no OneDrive. `path` aponta explicitamente pra raiz do
        // drive quando não há um id de pasta-pai para usar.
        body.insert(
            "parentReference".to_string(),
            match &request.new_parent_remote_item_id {
                Some(new_parent) => serde_json::json!({ "id": new_parent.as_ref() }),
                None => serde_json::json!({ "path": format!("/drives/{}/root:", request.namespace_remote_id) }),
            },
        );
        if let Some(new_name) = &request.new_name {
            body.insert("name".to_string(), serde_json::Value::String(new_name.clone()));
        }

        let mut builder = self
            .http
            .patch(&url)
            .bearer_auth(request.account.access_token.expose())
            .json(&body);
        if let Some(base_version) = &request.base_remote_version {
            builder = builder.header(reqwest::header::IF_MATCH, base_version.clone());
        }

        let response = builder.send().await.map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let item: GraphDriveItem = response.json().await.map_err(map_transport_error)?;
        Ok(map_drive_item(item))
    }

    async fn delete_item(&self, request: DeleteItemRequest) -> ProviderResult<()> {
        let url = self.item_url(&request.namespace_remote_id, request.remote_item_id.as_ref());
        let mut builder = self
            .http
            .request(Method::DELETE, &url)
            .bearer_auth(request.account.access_token.expose());
        if let Some(base_version) = &request.base_remote_version {
            builder = builder.header(reqwest::header::IF_MATCH, base_version.clone());
        }
        let response = builder.send().await.map_err(map_transport_error)?;
        ensure_success(response).await?;
        Ok(())
    }

    async fn restore_item(&self, request: RestoreItemRequest) -> ProviderResult<RemoteItem> {
        // Documentado publicamente apenas para OneDrive for Business/
        // SharePoint — contas pessoais tendem a receber `UnsupportedOperation`
        // aqui, o que a normalização de erro já expõe corretamente.
        let url = format!(
            "{GRAPH_BASE}/drives/{}/items/{}/restore",
            request.namespace_remote_id,
            request.remote_item_id.as_ref()
        );
        let response = self
            .http
            .post(&url)
            .bearer_auth(request.account.access_token.expose())
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(map_transport_error)?;
        let response = ensure_success(response).await?;
        let item: GraphDriveItem = response.json().await.map_err(map_transport_error)?;
        Ok(map_drive_item(item))
    }
}
