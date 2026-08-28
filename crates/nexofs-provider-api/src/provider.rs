//! Contrato principal de adaptador de nuvem. SPEC §5.1.

use crate::capabilities::ProviderCapabilities;
use crate::errors::ProviderResult;
use crate::model::{ChangePage, RemoteItem, RemoteNamespace, RemotePage};
use crate::requests::*;
use crate::secret::SecretToken;
use nexofs_domain::ProviderId;

#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub display_name: String,
    pub capabilities: ProviderCapabilities,
}

/// Implementado por cada adaptador de nuvem (OneDrive, Google Drive e futuramente outros) e pelo `nexofs-provider-fake` usado em testes determinísticos.
/// Nenhum chamador deste trait pode ignorar o `ProviderApiGovernor` — a
/// implementação da trait em si não impõe rate limiting; isso é
/// responsabilidade de quem a invoca (FR-API-001).
#[async_trait::async_trait]
pub trait CloudProvider: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;

    /// Monta a URL de autorização (endpoint + client_id + escopos são
    /// específicos do provedor); `state` e `pkce_challenge` vêm do fluxo
    /// genérico em `nexofs-auth`, que também abre o navegador e escuta o
    /// redirect loopback — o núcleo nunca constrói essa URL sozinho
    /// (SPEC §22.1, NFR-SEC-001).
    fn build_authorization_url(&self, redirect_uri: &str, state: &str, pkce_challenge: &str) -> String;

    async fn authenticate(
        &self,
        request: AuthenticationRequest,
    ) -> ProviderResult<AuthenticatedAccount>;

    async fn refresh_authentication(
        &self,
        account: &ProviderAccountContext,
    ) -> ProviderResult<AuthenticationState>;

    /// Troca um refresh token (guardado no keyring, NFR-SEC-003) por uma
    /// sessão de acesso nova — usado no boot do daemon para retomar contas
    /// já configuradas sem repetir o login interativo (FR-ACC-003/004).
    /// Distinto de `refresh_authentication`, que só verifica se o
    /// access_token corrente ainda vale (SPEC §5.1); este é o fluxo
    /// `grant_type=refresh_token` propriamente dito, específico de cada
    /// provedor (Fase 7, T7-02: generalizado do que antes era um método
    /// inerente só de `OneDriveProvider`, para `nexofsd` poder retomar
    /// qualquer provedor sem conhecer o tipo concreto por trás do `dyn
    /// CloudProvider`).
    async fn refresh_via_refresh_token(
        &self,
        refresh_token: &SecretToken,
    ) -> ProviderResult<AuthenticatedAccount>;

    async fn list_namespaces(
        &self,
        account: &ProviderAccountContext,
    ) -> ProviderResult<Vec<RemoteNamespace>>;

    async fn list_children(
        &self,
        request: ListChildrenRequest,
    ) -> ProviderResult<RemotePage<RemoteItem>>;

    async fn get_item(&self, request: GetItemRequest) -> ProviderResult<Option<RemoteItem>>;

    async fn create_change_cursor(
        &self,
        request: CreateCursorRequest,
    ) -> ProviderResult<crate::model::ChangeCursor>;

    async fn list_changes(&self, request: ListChangesRequest) -> ProviderResult<ChangePage>;

    async fn open_download(&self, request: DownloadRequest) -> ProviderResult<DownloadHandle>;

    async fn upload(&self, request: UploadRequest) -> ProviderResult<UploadResult>;

    async fn create_directory(
        &self,
        request: CreateDirectoryRequest,
    ) -> ProviderResult<RemoteItem>;

    async fn move_item(&self, request: MoveItemRequest) -> ProviderResult<RemoteItem>;

    async fn delete_item(&self, request: DeleteItemRequest) -> ProviderResult<()>;

    async fn restore_item(&self, request: RestoreItemRequest) -> ProviderResult<RemoteItem>;
}
