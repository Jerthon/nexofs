//! Decorador de `CloudProvider` que registra a ordem de chegada de cada
//! chamada — usado por testes de carga do scheduler (T3-11) para verificar
//! que o dispatcher respeita a ordem de prioridade sem precisar instrumentar
//! `nexofs-sync-core` internamente.

use async_trait::async_trait;
use nexofs_provider_api::*;
use std::sync::{Arc, Mutex};

pub struct RecordingProvider {
    inner: Arc<dyn CloudProvider>,
    calls: Mutex<Vec<&'static str>>,
}

impl RecordingProvider {
    pub fn new(inner: Arc<dyn CloudProvider>) -> Self {
        Self {
            inner,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Ordem em que as chamadas de mutação remota chegaram até aqui.
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, label: &'static str) {
        self.calls.lock().unwrap().push(label);
    }
}

#[async_trait]
impl CloudProvider for RecordingProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.inner.descriptor()
    }

    fn build_authorization_url(&self, redirect_uri: &str, state: &str, pkce_challenge: &str) -> String {
        self.inner.build_authorization_url(redirect_uri, state, pkce_challenge)
    }

    async fn authenticate(&self, request: AuthenticationRequest) -> ProviderResult<AuthenticatedAccount> {
        self.inner.authenticate(request).await
    }

    async fn refresh_authentication(&self, account: &ProviderAccountContext) -> ProviderResult<AuthenticationState> {
        self.inner.refresh_authentication(account).await
    }

    async fn refresh_via_refresh_token(&self, refresh_token: &SecretToken) -> ProviderResult<AuthenticatedAccount> {
        self.inner.refresh_via_refresh_token(refresh_token).await
    }

    async fn list_namespaces(&self, account: &ProviderAccountContext) -> ProviderResult<Vec<RemoteNamespace>> {
        self.inner.list_namespaces(account).await
    }

    async fn list_children(&self, request: ListChildrenRequest) -> ProviderResult<RemotePage<RemoteItem>> {
        self.inner.list_children(request).await
    }

    async fn get_item(&self, request: GetItemRequest) -> ProviderResult<Option<RemoteItem>> {
        self.inner.get_item(request).await
    }

    async fn create_change_cursor(&self, request: CreateCursorRequest) -> ProviderResult<ChangeCursor> {
        self.inner.create_change_cursor(request).await
    }

    async fn list_changes(&self, request: ListChangesRequest) -> ProviderResult<ChangePage> {
        self.inner.list_changes(request).await
    }

    async fn open_download(&self, request: DownloadRequest) -> ProviderResult<DownloadHandle> {
        self.inner.open_download(request).await
    }

    async fn upload(&self, request: UploadRequest) -> ProviderResult<UploadResult> {
        self.record("UPLOAD");
        self.inner.upload(request).await
    }

    async fn create_directory(&self, request: CreateDirectoryRequest) -> ProviderResult<RemoteItem> {
        self.record("CREATE_DIRECTORY");
        self.inner.create_directory(request).await
    }

    async fn move_item(&self, request: MoveItemRequest) -> ProviderResult<RemoteItem> {
        self.record("MOVE");
        self.inner.move_item(request).await
    }

    async fn delete_item(&self, request: DeleteItemRequest) -> ProviderResult<()> {
        self.record("DELETE");
        self.inner.delete_item(request).await
    }

    async fn restore_item(&self, request: RestoreItemRequest) -> ProviderResult<RemoteItem> {
        self.inner.restore_item(request).await
    }
}
