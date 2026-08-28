//! Tipos de domínio do NexoFS.
//!
//! Este crate NÃO DEVE depender de FUSE, SQLite, HTTP, Tauri ou de SDKs de
//! provedores específicos (SPEC §3.2, FR-MC-001). Ele existe para que o
//! núcleo de sincronização e os adaptadores de provedor compartilhem os
//! mesmos conceitos sem acoplamento a uma tecnologia de infraestrutura.

pub mod ids;
pub mod inode;
pub mod paths;
pub mod secret;
pub mod states;

pub use ids::{
    AccountId, ConflictId, Inode as InodeId, ItemId, NamespaceId, OperationId, ProviderId,
    RemoteItemId,
};
pub use secret::SecretToken;
pub use states::{
    ConflictResolution, ConflictType, CursorState, HydrationState, OperationState, OperationType,
    PinState, SyncDisposition, SyncState,
};
