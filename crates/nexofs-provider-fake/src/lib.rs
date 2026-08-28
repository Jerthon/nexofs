//! Provider simulado, sem rede, para testar o núcleo e o Governor de forma
//! determinística (FR-MC-004). Cobre o contrato `CloudProvider` inteiro.

mod fault_injector;
mod reader;
mod recording;

pub use fault_injector::FaultInjectingProvider;
pub use recording::RecordingProvider;

use async_trait::async_trait;
use nexofs_domain::{ProviderId, RemoteItemId};
use nexofs_provider_api::*;
use reader::InMemoryReader;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone)]
struct FakeItem {
    remote_item_id: String,
    parent_remote_item_id: Option<String>,
    name: String,
    kind: ItemKind,
    content: Vec<u8>,
    version: u64,
    deleted: bool,
}

impl FakeItem {
    fn to_remote_item(&self) -> RemoteItem {
        RemoteItem {
            remote_item_id: RemoteItemId::from(self.remote_item_id.clone()),
            parent_remote_item_id: self
                .parent_remote_item_id
                .clone()
                .map(RemoteItemId::from),
            name: self.name.clone(),
            kind: self.kind,
            size_bytes: self.content.len() as u64,
            mime_type: None,
            remote_version: Some(self.version.to_string()),
            remote_content_version: Some(self.version.to_string()),
            remote_modified_at_unix: None,
            remote_created_at_unix: None,
            provider_metadata_json: None,
        }
    }
}

#[derive(Default)]
struct NamespaceState {
    items: HashMap<String, FakeItem>,
    change_log: Vec<RemoteChange>,
    next_id: u64,
}

/// Provider em memória. Uma instância representa um único "tenant" simulado;
/// múltiplos namespaces podem coexistir (`namespace_remote_id` é a chave).
#[derive(Default)]
pub struct FakeProvider {
    state: Mutex<HashMap<String, NamespaceState>>,
}

const DEFAULT_NAMESPACE: &str = "fake-namespace";
const CHANGE_PAGE_SIZE: usize = 500;

impl FakeProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atalho de teste para inspecionar quantas entradas de mudança já
    /// foram geradas em um namespace.
    pub fn change_log_len(&self, namespace_remote_id: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .get(namespace_remote_id)
            .map(|ns| ns.change_log.len())
            .unwrap_or(0)
    }

    fn check_version(
        item: &FakeItem,
        base_remote_version: &Option<String>,
    ) -> ProviderResult<()> {
        if let Some(expected) = base_remote_version {
            if expected != &item.version.to_string() {
                return Err(ProviderError::new(
                    ProviderErrorKind::VersionConflict,
                    format!(
                        "versão base {expected} não confere com a versão atual {}",
                        item.version
                    ),
                ));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl CloudProvider for FakeProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::from("fake"),
            display_name: "Fake Provider (testes)".to_string(),
            capabilities: ProviderCapabilities {
                incremental_changes: true,
                latest_cursor_without_full_scan: true,
                push_notifications: false,
                metadata_batch: false,
                resumable_upload: false,
                ranged_download: false,
                stable_item_ids: true,
                content_version: true,
                metadata_version: true,
                remote_hashes: vec![],
                atomic_move: true,
                server_side_copy: false,
                trash: false,
                case_sensitivity: CaseSensitivity::Sensitive,
                max_simple_upload_bytes: None,
                max_item_name_bytes: None,
                max_path_bytes: None,
            },
        }
    }

    fn build_authorization_url(&self, redirect_uri: &str, state: &str, pkce_challenge: &str) -> String {
        format!("fake://authorize?redirect_uri={redirect_uri}&state={state}&pkce_challenge={pkce_challenge}")
    }

    async fn authenticate(
        &self,
        _request: AuthenticationRequest,
    ) -> ProviderResult<AuthenticatedAccount> {
        Ok(AuthenticatedAccount {
            provider_account_id: "fake-account".to_string(),
            display_name: "Conta simulada".to_string(),
            tenant_id: None,
            access_token: SecretToken::new("fake-access-token"),
            access_token_expires_at_unix: i64::MAX,
            refresh_token: SecretToken::new("fake-refresh-token"),
        })
    }

    async fn refresh_authentication(
        &self,
        _account: &ProviderAccountContext,
    ) -> ProviderResult<AuthenticationState> {
        Ok(AuthenticationState::Valid {
            expires_at_unix: i64::MAX,
        })
    }

    async fn refresh_via_refresh_token(&self, _refresh_token: &SecretToken) -> ProviderResult<AuthenticatedAccount> {
        Ok(AuthenticatedAccount {
            provider_account_id: "fake-account".to_string(),
            display_name: "Conta simulada".to_string(),
            tenant_id: None,
            access_token: SecretToken::new("fake-access-token"),
            access_token_expires_at_unix: i64::MAX,
            refresh_token: SecretToken::new("fake-refresh-token"),
        })
    }

    async fn list_namespaces(
        &self,
        _account: &ProviderAccountContext,
    ) -> ProviderResult<Vec<RemoteNamespace>> {
        Ok(vec![RemoteNamespace {
            remote_namespace_id: DEFAULT_NAMESPACE.to_string(),
            display_name: "Namespace simulado".to_string(),
            kind: NamespaceKind::Personal,
        }])
    }

    async fn list_children(
        &self,
        request: ListChildrenRequest,
    ) -> ProviderResult<RemotePage<RemoteItem>> {
        let state = self.state.lock().unwrap();
        let parent = request.parent_remote_item_id.map(|id| id.0);
        let items = state
            .get(&request.namespace_remote_id)
            .map(|ns| {
                ns.items
                    .values()
                    .filter(|item| !item.deleted && item.parent_remote_item_id == parent)
                    .map(FakeItem::to_remote_item)
                    .collect()
            })
            .unwrap_or_default();
        Ok(RemotePage {
            items,
            next_page_token: None,
        })
    }

    async fn get_item(&self, request: GetItemRequest) -> ProviderResult<Option<RemoteItem>> {
        let state = self.state.lock().unwrap();
        let item = state
            .get(&request.namespace_remote_id)
            .and_then(|ns| ns.items.get(request.remote_item_id.as_ref()))
            .filter(|item| !item.deleted)
            .map(FakeItem::to_remote_item);
        Ok(item)
    }

    async fn create_change_cursor(
        &self,
        request: CreateCursorRequest,
    ) -> ProviderResult<ChangeCursor> {
        let state = self.state.lock().unwrap();
        let log_len = state
            .get(&request.namespace_remote_id)
            .map(|ns| ns.change_log.len())
            .unwrap_or(0);
        // `latest_only` pula o histórico já existente (indexação lazy,
        // FR-IDX-004); caso contrário o cursor "0" replay todo o histórico,
        // equivalente a uma reconstrução completa da árvore.
        let offset = if request.latest_only { log_len } else { 0 };
        Ok(ChangeCursor(offset.to_string()))
    }

    async fn list_changes(&self, request: ListChangesRequest) -> ProviderResult<ChangePage> {
        let offset: usize = request.cursor.0.parse().map_err(|_| {
            ProviderError::new(ProviderErrorKind::CorruptResponse, "cursor inválido")
        })?;
        let state = self.state.lock().unwrap();
        let log = state
            .get(&request.namespace_remote_id)
            .map(|ns| ns.change_log.as_slice())
            .unwrap_or(&[]);

        if offset > log.len() {
            return Err(ProviderError::new(
                ProviderErrorKind::CorruptResponse,
                "cursor além do histórico conhecido — requer reconciliação",
            ));
        }

        let remaining = &log[offset..];
        let take = remaining.len().min(CHANGE_PAGE_SIZE);
        let changes = remaining[..take].to_vec();
        let next_offset = offset + take;

        Ok(ChangePage {
            changes,
            next_cursor: ChangeCursor(next_offset.to_string()),
            has_more: next_offset < log.len(),
        })
    }

    async fn open_download(&self, request: DownloadRequest) -> ProviderResult<DownloadHandle> {
        let state = self.state.lock().unwrap();
        let item = state
            .get(&request.namespace_remote_id)
            .and_then(|ns| ns.items.get(request.remote_item_id.as_ref()))
            .filter(|item| !item.deleted && item.kind == ItemKind::File)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::NotFound, "item não encontrado"))?;

        let content = match request.range {
            Some(range) => item
                .content
                .get(range.start as usize..=range.end as usize)
                .map(|slice| slice.to_vec())
                .ok_or_else(|| {
                    ProviderError::new(ProviderErrorKind::InvalidName, "range fora dos limites")
                })?,
            None => item.content.clone(),
        };

        Ok(DownloadHandle {
            content_length: Some(content.len() as u64),
            remote_content_version: Some(item.version.to_string()),
            reader: Box::pin(InMemoryReader::new(content)),
        })
    }

    async fn upload(&self, mut request: UploadRequest) -> ProviderResult<UploadResult> {
        let mut content = Vec::with_capacity(request.size_bytes as usize);
        request
            .content
            .read_to_end(&mut content)
            .await
            .map_err(|e| ProviderError::new(ProviderErrorKind::Network, e.to_string()))?;

        let mut state = self.state.lock().unwrap();
        let ns = state.entry(request.namespace_remote_id.clone()).or_default();

        let existing_id = ns
            .items
            .values()
            .find(|i| {
                !i.deleted
                    && i.name == request.name
                    && i.parent_remote_item_id
                        == request.parent_remote_item_id.as_ref().map(|id| id.0.clone())
            })
            .map(|i| i.remote_item_id.clone());

        let item = if let Some(id) = existing_id {
            let item = ns.items.get_mut(&id).expect("id veio do próprio mapa");
            FakeProvider::check_version(item, &request.base_remote_version)?;
            item.content = content;
            item.version += 1;
            item.clone()
        } else {
            ns.next_id += 1;
            let id = format!("fake-item-{}", ns.next_id);
            let item = FakeItem {
                remote_item_id: id.clone(),
                parent_remote_item_id: request.parent_remote_item_id.map(|p| p.0),
                name: request.name,
                kind: ItemKind::File,
                content,
                version: 1,
                deleted: false,
            };
            ns.items.insert(id, item.clone());
            item
        };

        let remote_item = item.to_remote_item();
        ns.change_log.push(RemoteChange::Upserted(remote_item.clone()));

        Ok(UploadResult {
            item: remote_item,
            resumable_session_token: None,
        })
    }

    async fn create_directory(
        &self,
        request: CreateDirectoryRequest,
    ) -> ProviderResult<RemoteItem> {
        let mut state = self.state.lock().unwrap();
        let ns = state.entry(request.namespace_remote_id).or_default();
        ns.next_id += 1;
        let id = format!("fake-item-{}", ns.next_id);
        let item = FakeItem {
            remote_item_id: id.clone(),
            parent_remote_item_id: request.parent_remote_item_id.map(|p| p.0),
            name: request.name,
            kind: ItemKind::Directory,
            content: Vec::new(),
            version: 1,
            deleted: false,
        };
        ns.items.insert(id, item.clone());
        let remote_item = item.to_remote_item();
        ns.change_log.push(RemoteChange::Upserted(remote_item.clone()));
        Ok(remote_item)
    }

    async fn move_item(&self, request: MoveItemRequest) -> ProviderResult<RemoteItem> {
        let mut state = self.state.lock().unwrap();
        let ns = state
            .get_mut(&request.namespace_remote_id)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::NotFound, "namespace desconhecido"))?;
        let item = ns
            .items
            .get_mut(request.remote_item_id.as_ref())
            .filter(|i| !i.deleted)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::NotFound, "item não encontrado"))?;

        FakeProvider::check_version(item, &request.base_remote_version)?;

        // `None` significa "raiz", não "não mudar o pai" — mesma
        // convenção do provedor OneDrive real (ver o bug irmão corrigido
        // em `nexofs-provider-onedrive::move_item`). Sem isto o
        // `FakeProvider` mascarava exatamente o bug que o provedor real
        // tinha.
        item.parent_remote_item_id = request.new_parent_remote_item_id.map(|id| id.0);
        if let Some(new_name) = request.new_name {
            item.name = new_name;
        }
        item.version += 1;
        let remote_item = item.to_remote_item();
        ns.change_log.push(RemoteChange::Upserted(remote_item.clone()));
        Ok(remote_item)
    }

    async fn delete_item(&self, request: DeleteItemRequest) -> ProviderResult<()> {
        let mut state = self.state.lock().unwrap();
        let ns = state
            .get_mut(&request.namespace_remote_id)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::NotFound, "namespace desconhecido"))?;
        let item = ns
            .items
            .get_mut(request.remote_item_id.as_ref())
            .filter(|i| !i.deleted)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::NotFound, "item não encontrado"))?;

        FakeProvider::check_version(item, &request.base_remote_version)?;
        item.deleted = true;
        item.version += 1;
        ns.change_log.push(RemoteChange::Deleted {
            remote_item_id: request.remote_item_id,
        });
        Ok(())
    }

    async fn restore_item(&self, request: RestoreItemRequest) -> ProviderResult<RemoteItem> {
        let mut state = self.state.lock().unwrap();
        let ns = state
            .get_mut(&request.namespace_remote_id)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::NotFound, "namespace desconhecido"))?;
        let item = ns
            .items
            .get_mut(request.remote_item_id.as_ref())
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::NotFound, "item não encontrado"))?;

        item.deleted = false;
        item.version += 1;
        let remote_item = item.to_remote_item();
        ns.change_log.push(RemoteChange::Upserted(remote_item.clone()));
        Ok(remote_item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ProviderAccountContext {
        ProviderAccountContext {
            account_id: nexofs_domain::AccountId::new(),
            provider_account_id: "fake-account".to_string(),
            tenant_id: None,
            access_token: SecretToken::new("token"),
        }
    }

    #[tokio::test]
    async fn upload_then_download_roundtrip() {
        let provider = FakeProvider::new();
        let result = provider
            .upload(UploadRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                parent_remote_item_id: None,
                name: "arquivo.txt".to_string(),
                size_bytes: 5,
                base_remote_version: None,
                content: Box::pin(InMemoryReader::new(b"hello".to_vec())),
                resumable_session_token: None,
            })
            .await
            .unwrap();

        let handle = provider
            .open_download(DownloadRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                remote_item_id: result.item.remote_item_id,
                range: None,
            })
            .await
            .unwrap();

        let mut buf = Vec::new();
        let mut reader = handle.reader;
        reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"hello");
    }

    #[tokio::test]
    async fn optimistic_concurrency_detects_conflict() {
        let provider = FakeProvider::new();
        let created = provider
            .upload(UploadRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                parent_remote_item_id: None,
                name: "a.txt".to_string(),
                size_bytes: 1,
                base_remote_version: None,
                content: Box::pin(InMemoryReader::new(b"a".to_vec())),
                resumable_session_token: None,
            })
            .await
            .unwrap();

        // Sobrescreve usando uma base_remote_version desatualizada.
        let stale_result = provider
            .upload(UploadRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                parent_remote_item_id: None,
                name: "a.txt".to_string(),
                size_bytes: 1,
                base_remote_version: Some("999".to_string()),
                content: Box::pin(InMemoryReader::new(b"b".to_vec())),
                resumable_session_token: None,
            })
            .await;

        assert!(matches!(
            stale_result.unwrap_err().kind,
            ProviderErrorKind::VersionConflict
        ));
        assert_eq!(created.item.remote_version.unwrap(), "1");
    }

    #[tokio::test]
    async fn delta_replays_full_history_from_zero_cursor() {
        let provider = FakeProvider::new();
        provider
            .create_directory(CreateDirectoryRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                parent_remote_item_id: None,
                name: "pasta".to_string(),
            })
            .await
            .unwrap();

        let cursor = provider
            .create_change_cursor(CreateCursorRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                latest_only: false,
            })
            .await
            .unwrap();
        assert_eq!(cursor.0, "0");

        let page = provider
            .list_changes(ListChangesRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                cursor,
            })
            .await
            .unwrap();

        assert_eq!(page.changes.len(), 1);
        assert!(!page.has_more);
    }

    #[tokio::test]
    async fn latest_only_cursor_skips_existing_history() {
        let provider = FakeProvider::new();
        provider
            .create_directory(CreateDirectoryRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                parent_remote_item_id: None,
                name: "pasta-antiga".to_string(),
            })
            .await
            .unwrap();

        let cursor = provider
            .create_change_cursor(CreateCursorRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                latest_only: true,
            })
            .await
            .unwrap();

        let page = provider
            .list_changes(ListChangesRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                cursor,
            })
            .await
            .unwrap();

        assert!(page.changes.is_empty());
    }

    #[tokio::test]
    async fn create_then_delete_marks_tombstone_and_hides_from_listing() {
        let provider = FakeProvider::new();
        let item = provider
            .create_directory(CreateDirectoryRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                parent_remote_item_id: None,
                name: "temp".to_string(),
            })
            .await
            .unwrap();

        provider
            .delete_item(DeleteItemRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                remote_item_id: item.remote_item_id.clone(),
                base_remote_version: None,
            })
            .await
            .unwrap();

        let listing = provider
            .list_children(ListChildrenRequest {
                account: ctx(),
                namespace_remote_id: DEFAULT_NAMESPACE.to_string(),
                parent_remote_item_id: None,
                page_token: None,
            })
            .await
            .unwrap();

        assert!(listing.items.is_empty());
    }
}
