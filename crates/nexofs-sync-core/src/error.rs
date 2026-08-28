use nexofs_metadata_store::StoreError;
use nexofs_provider_api::ProviderError;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("erro no índice local: {0}")]
    Store(#[from] StoreError),
    #[error("erro do provedor: {0}")]
    Provider(#[from] ProviderError),
    #[error("erro no cache de conteúdo: {0}")]
    Cache(#[from] nexofs_content_cache::CacheError),
    #[error("erro no overlay local-only: {0}")]
    Overlay(#[from] nexofs_overlay::OverlayError),
    #[error("item não encontrado no índice local")]
    NotFound,
    #[error("operação inválida: {0}")]
    InvalidOperation(&'static str),
    #[error("já existe um item com esse nome neste diretório")]
    AlreadyExists,
    #[error("diretório não está vazio")]
    NotEmpty,
    #[error("não é um diretório")]
    NotADirectory,
    #[error("é um diretório")]
    IsADirectory,
    #[error("erro de I/O: {0}")]
    Io(#[from] std::io::Error),
}
