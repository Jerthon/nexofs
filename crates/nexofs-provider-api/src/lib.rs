//! Contrato provider-neutral para adaptadores de nuvem.
//!
//! O núcleo (`nexofs-sync-core`) só conhece os tipos deste crate — nunca um
//! tipo do SDK do Microsoft Graph, Google ou Dropbox (FR-MC-001).

pub mod capabilities;
pub mod errors;
pub mod model;
pub mod provider;
pub mod requests;
pub mod secret;

pub use capabilities::{CaseSensitivity, HashAlgorithm, ProviderCapabilities};
pub use errors::{ProviderError, ProviderErrorKind, ProviderResult};
pub use model::{
    ByteRange, ChangeCursor, ChangePage, ItemKind, NamespaceKind, RemoteChange, RemoteItem,
    RemoteNamespace, RemotePage,
};
pub use provider::{CloudProvider, ProviderDescriptor};
pub use requests::*;
pub use secret::SecretToken;
