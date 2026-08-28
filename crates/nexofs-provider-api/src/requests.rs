//! Requests/responses do contrato `CloudProvider`. SPEC §5.1.

use crate::model::{ByteRange, ChangeCursor, RemoteItem};
use crate::secret::SecretToken;
use nexofs_domain::{AccountId, RemoteItemId};
use std::pin::Pin;
use tokio::io::AsyncRead;

/// Contexto de credenciais já resolvido pelo `nexofs-auth` — o adaptador
/// nunca lê o keyring diretamente (NFR-SEC-003).
#[derive(Debug, Clone)]
pub struct ProviderAccountContext {
    pub account_id: AccountId,
    pub provider_account_id: String,
    pub tenant_id: Option<String>,
    pub access_token: SecretToken,
}

#[derive(Debug, Clone)]
pub struct AuthenticationRequest {
    pub authorization_code: SecretToken,
    pub pkce_verifier: SecretToken,
    pub redirect_uri: String,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedAccount {
    pub provider_account_id: String,
    pub display_name: String,
    pub tenant_id: Option<String>,
    pub access_token: SecretToken,
    pub access_token_expires_at_unix: i64,
    pub refresh_token: SecretToken,
}

#[derive(Debug, Clone)]
pub enum AuthenticationState {
    Valid { expires_at_unix: i64 },
    RequiresReauthentication,
}

#[derive(Debug, Clone)]
pub struct ListChildrenRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub parent_remote_item_id: Option<RemoteItemId>,
    pub page_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GetItemRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub remote_item_id: RemoteItemId,
}

/// `latest_only = true` pede um cursor "a partir de agora" (FR-IDX-004),
/// preservando a indexação lazy em vez de forçar varredura integral.
#[derive(Debug, Clone)]
pub struct CreateCursorRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub latest_only: bool,
}

#[derive(Debug, Clone)]
pub struct ListChangesRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub cursor: ChangeCursor,
}

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub remote_item_id: RemoteItemId,
    pub range: Option<ByteRange>,
}

pub struct DownloadHandle {
    pub reader: Pin<Box<dyn AsyncRead + Send>>,
    pub content_length: Option<u64>,
    pub remote_content_version: Option<String>,
}

impl std::fmt::Debug for DownloadHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadHandle")
            .field("content_length", &self.content_length)
            .field("remote_content_version", &self.remote_content_version)
            .finish_non_exhaustive()
    }
}

/// `base_remote_version` habilita o precondition de escrita otimista
/// (FR-UP-006): divergência da base vira conflito, nunca overwrite silencioso.
pub struct UploadRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub parent_remote_item_id: Option<RemoteItemId>,
    pub name: String,
    pub size_bytes: u64,
    pub base_remote_version: Option<String>,
    pub content: Pin<Box<dyn AsyncRead + Send>>,
    /// Token opaco de sessão resumível já em andamento, se houver (FR-UP-004).
    pub resumable_session_token: Option<String>,
}

impl std::fmt::Debug for UploadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadRequest")
            .field("namespace_remote_id", &self.namespace_remote_id)
            .field("name", &self.name)
            .field("size_bytes", &self.size_bytes)
            .field("base_remote_version", &self.base_remote_version)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct UploadResult {
    pub item: RemoteItem,
    /// Presente quando o upload precisou ser retomado depois (sessão ainda aberta).
    pub resumable_session_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CreateDirectoryRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub parent_remote_item_id: Option<RemoteItemId>,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct MoveItemRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub remote_item_id: RemoteItemId,
    pub new_parent_remote_item_id: Option<RemoteItemId>,
    pub new_name: Option<String>,
    pub base_remote_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DeleteItemRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub remote_item_id: RemoteItemId,
    pub base_remote_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RestoreItemRequest {
    pub account: ProviderAccountContext,
    pub namespace_remote_id: String,
    pub remote_item_id: RemoteItemId,
}
