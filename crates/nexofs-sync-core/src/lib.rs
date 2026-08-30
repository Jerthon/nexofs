//! Núcleo genérico de sincronização — orquestra índice local, Governor,
//! provider e cache de conteúdo. Não conhece FUSE nem nenhum SDK de
//! provedor específico (SPEC §3.2).

mod core;
mod error;
mod events;
mod model;
mod per_key_lock;
mod queries;

pub use crate::core::{SyncCore, SyncCoreContext};
pub use error::SyncError;
pub use events::{EventBus, SyncEvent};
pub use crate::core::{ConflictSummary, NamespaceDiagnostics, OperationsFilter, OperationsPage};
pub use model::{CacheBreakdown, CacheStats, IndexedItem, QueuedOperation};
pub use nexofs_content_cache::DiskPressureLevel;
pub use nexofs_domain::states::{ConflictResolution, ConflictType, PinState};
pub use nexofs_domain::ConflictId;
pub use nexofs_ignore::{Profile as IgnoreProfile, Rule as IgnoreRule, RuleTier, KNOWN_PROFILES};
