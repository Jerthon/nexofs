//! Taxonomia normalizada de erros de provedor. SPEC §5.3.
//!
//! Cada adaptador DEVE converter respostas específicas (códigos HTTP,
//! payloads de erro do Graph/Google/Dropbox) para esta taxonomia — o núcleo
//! nunca inspeciona um código de status específico de provedor.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderErrorKind {
    AuthenticationRequired,
    AuthorizationDenied,
    NotFound,
    AlreadyExists,
    VersionConflict,
    RateLimited { retry_after: Option<Duration> },
    TemporarilyUnavailable { retry_after: Option<Duration> },
    QuotaExceeded,
    InvalidName,
    UnsupportedOperation,
    Network,
    Timeout,
    CorruptResponse,
    Permanent,
}

#[derive(Debug, Error, Clone)]
#[error("{kind:?}: {message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Indica se o Governor deve reter novas chamadas no mesmo escopo até
    /// `Retry-After`, quando presente (SPEC §7.3/§7.7).
    pub fn retry_after(&self) -> Option<Duration> {
        match &self.kind {
            ProviderErrorKind::RateLimited { retry_after }
            | ProviderErrorKind::TemporarilyUnavailable { retry_after } => *retry_after,
            _ => None,
        }
    }

    pub fn is_transient(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::RateLimited { .. }
                | ProviderErrorKind::TemporarilyUnavailable { .. }
                | ProviderErrorKind::Network
                | ProviderErrorKind::Timeout
        )
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;
