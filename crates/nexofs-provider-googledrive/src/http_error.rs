//! Normalização de respostas HTTP da Drive API para `ProviderErrorKind`.
//! SPEC §5.3. Google usa 403 tanto para "sem permissão" quanto para os
//! vários tipos de limite de taxa — só o campo `errors[].reason` do corpo
//! distingue, ao contrário do Graph (que usa 429 dedicado para throttling).

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

const RATE_LIMIT_REASONS: &[&str] = &["userRateLimitExceeded", "rateLimitExceeded", "dailyLimitExceeded"];

/// Parte pura de `ensure_success` — separada só para ser testável sem um
/// `reqwest::Response` de verdade (o construtor dele não é público fora do
/// crate `reqwest`, então um teste de unidade não consegue montar um; ver
/// `#[cfg(test)]` abaixo). T6-04/SPEC §26.3: cobre a normalização de
/// 401/403/404/409/412/429/503.
fn classify_error(status: StatusCode, retry_after: Option<Duration>, reason: &str) -> ProviderErrorKind {
    match status {
        StatusCode::UNAUTHORIZED => ProviderErrorKind::AuthenticationRequired,
        StatusCode::FORBIDDEN if reason == "storageQuotaExceeded" => ProviderErrorKind::QuotaExceeded,
        StatusCode::FORBIDDEN if RATE_LIMIT_REASONS.contains(&reason) => ProviderErrorKind::RateLimited { retry_after },
        StatusCode::FORBIDDEN => ProviderErrorKind::AuthorizationDenied,
        StatusCode::NOT_FOUND => ProviderErrorKind::NotFound,
        StatusCode::CONFLICT => ProviderErrorKind::AlreadyExists,
        StatusCode::PRECONDITION_FAILED => ProviderErrorKind::VersionConflict,
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorKind::RateLimited { retry_after },
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT | StatusCode::INTERNAL_SERVER_ERROR => {
            ProviderErrorKind::TemporarilyUnavailable { retry_after }
        }
        StatusCode::REQUEST_TIMEOUT => ProviderErrorKind::Timeout,
        s if s.is_client_error() => ProviderErrorKind::Permanent,
        _ => ProviderErrorKind::TemporarilyUnavailable { retry_after },
    }
}

/// Consome a resposta HTTP inteira, retornando `Ok(Response)` só quando o
/// status é de sucesso — caso contrário lê o corpo de erro do Google
/// (`{"error": {"code", "message", "errors": [{"reason"}]}}`) e converte
/// para a taxonomia neutra.
pub async fn ensure_success(response: Response) -> Result<Response, ProviderError> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let retry = retry_after(&response);
    let body_text = response.text().await.unwrap_or_default();
    let parsed = serde_json::from_str::<crate::dto::GoogleErrorBody>(&body_text).ok();
    let message = parsed
        .as_ref()
        .map(|b| b.error.message.clone())
        .unwrap_or_else(|| body_text.clone());
    let reason = parsed.as_ref().and_then(|b| b.error.errors.first()).map(|r| r.reason.as_str()).unwrap_or("");

    Err(ProviderError::new(classify_error(status, retry, reason), message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_authentication_and_authorization_errors() {
        assert_eq!(classify_error(StatusCode::UNAUTHORIZED, None, ""), ProviderErrorKind::AuthenticationRequired);
        assert_eq!(classify_error(StatusCode::FORBIDDEN, None, ""), ProviderErrorKind::AuthorizationDenied);
    }

    #[test]
    fn distinguishes_the_three_forbidden_reasons_google_overloads_into_403() {
        assert_eq!(classify_error(StatusCode::FORBIDDEN, None, "storageQuotaExceeded"), ProviderErrorKind::QuotaExceeded);
        assert_eq!(
            classify_error(StatusCode::FORBIDDEN, Some(Duration::from_secs(30)), "userRateLimitExceeded"),
            ProviderErrorKind::RateLimited { retry_after: Some(Duration::from_secs(30)) }
        );
        assert_eq!(classify_error(StatusCode::FORBIDDEN, None, "dailyLimitExceeded"), ProviderErrorKind::RateLimited { retry_after: None });
        assert_eq!(classify_error(StatusCode::FORBIDDEN, None, "insufficientPermissions"), ProviderErrorKind::AuthorizationDenied);
    }

    #[test]
    fn maps_not_found_conflict_and_version_conflict() {
        assert_eq!(classify_error(StatusCode::NOT_FOUND, None, ""), ProviderErrorKind::NotFound);
        assert_eq!(classify_error(StatusCode::CONFLICT, None, ""), ProviderErrorKind::AlreadyExists);
        assert_eq!(classify_error(StatusCode::PRECONDITION_FAILED, None, ""), ProviderErrorKind::VersionConflict);
    }

    #[test]
    fn maps_rate_limiting_and_transient_server_errors_with_retry_after() {
        let retry = Some(Duration::from_secs(5));
        assert_eq!(classify_error(StatusCode::TOO_MANY_REQUESTS, retry, ""), ProviderErrorKind::RateLimited { retry_after: retry });
        assert_eq!(classify_error(StatusCode::SERVICE_UNAVAILABLE, retry, ""), ProviderErrorKind::TemporarilyUnavailable { retry_after: retry });
        assert_eq!(classify_error(StatusCode::GATEWAY_TIMEOUT, retry, ""), ProviderErrorKind::TemporarilyUnavailable { retry_after: retry });
        assert_eq!(classify_error(StatusCode::INTERNAL_SERVER_ERROR, retry, ""), ProviderErrorKind::TemporarilyUnavailable { retry_after: retry });
    }

    #[test]
    fn an_unrecognized_client_error_becomes_permanent_not_a_silent_retry_loop() {
        assert_eq!(classify_error(StatusCode::BAD_REQUEST, None, ""), ProviderErrorKind::Permanent);
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
