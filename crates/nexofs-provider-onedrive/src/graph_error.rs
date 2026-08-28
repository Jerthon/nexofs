//! Normalização de respostas HTTP do Graph para `ProviderErrorKind`.
//! SPEC §5.3, §6.2 "normalização de erros e throttling".

use nexofs_provider_api::{ProviderError, ProviderErrorKind};
use reqwest::{Response, StatusCode};
use std::time::Duration;

fn retry_after(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Parte pura de `ensure_success` — separada só para ser testável sem um
/// `reqwest::Response` de verdade (o construtor dele não é público fora do
/// crate `reqwest`, então um teste de unidade não consegue montar um; ver
/// `#[cfg(test)]` abaixo). T6-04/SPEC §26.3: cobre a normalização de
/// 401/403/404/409/412/429/503.
fn classify_error(status: StatusCode, retry_after: Option<Duration>) -> ProviderErrorKind {
    match status {
        StatusCode::UNAUTHORIZED => ProviderErrorKind::AuthenticationRequired,
        StatusCode::FORBIDDEN => ProviderErrorKind::AuthorizationDenied,
        StatusCode::NOT_FOUND => ProviderErrorKind::NotFound,
        StatusCode::CONFLICT => ProviderErrorKind::AlreadyExists,
        StatusCode::PRECONDITION_FAILED => ProviderErrorKind::VersionConflict,
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorKind::RateLimited { retry_after },
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => ProviderErrorKind::TemporarilyUnavailable { retry_after },
        StatusCode::INSUFFICIENT_STORAGE => ProviderErrorKind::QuotaExceeded,
        StatusCode::REQUEST_TIMEOUT => ProviderErrorKind::Timeout,
        s if s.is_client_error() => ProviderErrorKind::Permanent,
        _ => ProviderErrorKind::TemporarilyUnavailable { retry_after },
    }
}

/// Consome a resposta HTTP inteira, retornando `Ok(Response)` só quando o
/// status é de sucesso — caso contrário lê o corpo de erro do Graph
/// (`{"error": {"code", "message"}}`) e converte para a taxonomia neutra.
pub async fn ensure_success(response: Response) -> Result<Response, ProviderError> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let retry = retry_after(&response);
    let body_text = response.text().await.unwrap_or_default();
    let graph_message = serde_json::from_str::<crate::dto::GraphErrorBody>(&body_text)
        .map(|b| format!("{}: {}", b.error.code, b.error.message))
        .unwrap_or(body_text);

    Err(ProviderError::new(classify_error(status, retry), graph_message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_authentication_authorization_and_not_found() {
        assert_eq!(classify_error(StatusCode::UNAUTHORIZED, None), ProviderErrorKind::AuthenticationRequired);
        assert_eq!(classify_error(StatusCode::FORBIDDEN, None), ProviderErrorKind::AuthorizationDenied);
        assert_eq!(classify_error(StatusCode::NOT_FOUND, None), ProviderErrorKind::NotFound);
    }

    #[test]
    fn maps_conflict_and_version_conflict() {
        assert_eq!(classify_error(StatusCode::CONFLICT, None), ProviderErrorKind::AlreadyExists);
        assert_eq!(classify_error(StatusCode::PRECONDITION_FAILED, None), ProviderErrorKind::VersionConflict);
    }

    #[test]
    fn maps_rate_limiting_and_transient_server_errors_with_retry_after() {
        let retry = Some(Duration::from_secs(5));
        assert_eq!(classify_error(StatusCode::TOO_MANY_REQUESTS, retry), ProviderErrorKind::RateLimited { retry_after: retry });
        assert_eq!(classify_error(StatusCode::SERVICE_UNAVAILABLE, retry), ProviderErrorKind::TemporarilyUnavailable { retry_after: retry });
        assert_eq!(classify_error(StatusCode::GATEWAY_TIMEOUT, retry), ProviderErrorKind::TemporarilyUnavailable { retry_after: retry });
    }

    #[test]
    fn maps_insufficient_storage_to_quota_exceeded() {
        assert_eq!(classify_error(StatusCode::INSUFFICIENT_STORAGE, None), ProviderErrorKind::QuotaExceeded);
    }

    #[test]
    fn an_unrecognized_client_error_becomes_permanent_not_a_silent_retry_loop() {
        assert_eq!(classify_error(StatusCode::BAD_REQUEST, None), ProviderErrorKind::Permanent);
    }
}

pub fn map_transport_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::new(ProviderErrorKind::Timeout, error.to_string())
    } else if error.is_decode() {
        ProviderError::new(ProviderErrorKind::CorruptResponse, error.to_string())
    } else {
        ProviderError::new(ProviderErrorKind::Network, error.to_string())
    }
}
