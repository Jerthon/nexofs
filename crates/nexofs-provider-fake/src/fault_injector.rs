//! Decorador de `CloudProvider` para testes de fault injection (T3-09,
//! T3-10, SPEC §26.3) — envolve qualquer provedor (tipicamente `FakeProvider`)
//! e permite enfileirar falhas a serem devolvidas nas próximas chamadas que
//! normalmente iriam à rede, sem tocar o provedor real por trás. Autenticação
//! e metadados estáticos (`descriptor`/`build_authorization_url`) nunca
//! passam pelo Governor no núcleo — não faz sentido injetar falha neles, e
//! não fazê-lo mantém o decorador simples.

use async_trait::async_trait;
use nexofs_provider_api::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub struct FaultInjectingProvider {
    inner: Arc<dyn CloudProvider>,
    pending_failures: Mutex<VecDeque<ProviderErrorKind>>,
}

impl FaultInjectingProvider {
    pub fn new(inner: Arc<dyn CloudProvider>) -> Self {
        Self {
            inner,
            pending_failures: Mutex::new(VecDeque::new()),
        }
    }

    /// Enfileira uma falha a ser devolvida na próxima chamada governada
    /// (FIFO — várias chamadas em sequência consomem uma cada).
    pub fn queue_failure(&self, kind: ProviderErrorKind) {
        self.pending_failures.lock().unwrap().push_back(kind);
    }

    /// Atalho para simular N falhas de rede seguidas (queda de conexão) —
    /// o cenário mais comum nos testes de T3-09/T3-10.
    pub fn queue_network_failures(&self, count: u32) {
        for _ in 0..count {
            self.queue_failure(ProviderErrorKind::Network);
        }
    }

    fn maybe_fail(&self) -> Option<ProviderError> {
        let kind = self.pending_failures.lock().unwrap().pop_front()?;
        Some(ProviderError::new(kind, "falha injetada por FaultInjectingProvider (teste)"))
    }
}

#[async_trait]
impl CloudProvider for FaultInjectingProvider {
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
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.list_namespaces(account).await
    }

    async fn list_children(&self, request: ListChildrenRequest) -> ProviderResult<RemotePage<RemoteItem>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.list_children(request).await
    }

    async fn get_item(&self, request: GetItemRequest) -> ProviderResult<Option<RemoteItem>> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.get_item(request).await
    }

    async fn create_change_cursor(&self, request: CreateCursorRequest) -> ProviderResult<ChangeCursor> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.create_change_cursor(request).await
    }

    async fn list_changes(&self, request: ListChangesRequest) -> ProviderResult<ChangePage> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.list_changes(request).await
    }

    async fn open_download(&self, request: DownloadRequest) -> ProviderResult<DownloadHandle> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.open_download(request).await
    }

    async fn upload(&self, request: UploadRequest) -> ProviderResult<UploadResult> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.upload(request).await
    }

    async fn create_directory(&self, request: CreateDirectoryRequest) -> ProviderResult<RemoteItem> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.create_directory(request).await
    }

    async fn move_item(&self, request: MoveItemRequest) -> ProviderResult<RemoteItem> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.move_item(request).await
    }

    async fn delete_item(&self, request: DeleteItemRequest) -> ProviderResult<()> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.delete_item(request).await
    }

    async fn restore_item(&self, request: RestoreItemRequest) -> ProviderResult<RemoteItem> {
        if let Some(err) = self.maybe_fail() {
            return Err(err);
        }
        self.inner.restore_item(request).await
    }
}
