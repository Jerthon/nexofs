//! Redirect loopback local para o fluxo Authorization Code + PKCE
//! (PRD §4.2 "Login ocorre no navegador do sistema"; NFR-SEC-001).

use crate::error::AuthError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Página de confirmação pós-login (SPEC §4.2). Fecha sozinha 3s depois de
/// carregar — `window.close()` só funciona em abas abertas por script (é
/// exatamente o caso aqui: o navegador abriu isto a partir do redirect do
/// fluxo OAuth), então o botão "Fechar" fica de reforço para quando o
/// navegador bloquear o fechamento automático.
const SUCCESS_BODY: &str = include_str!("loopback_success.html");

const ERROR_BODY: &str = include_str!("loopback_error.html");

pub struct AuthorizationCode {
    pub code: String,
    pub state: String,
}

pub struct LoopbackListener {
    listener: TcpListener,
    port: u16,
}

impl LoopbackListener {
    /// Vincula uma porta efêmera em `127.0.0.1` — nunca expõe o listener na
    /// rede, apenas ao próprio navegador local (SPEC §22.1 "loopback redirect
    /// local").
    pub async fn bind() -> Result<Self, AuthError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    /// `http://localhost:{porta}` — sem caminho e usando o hostname
    /// `localhost` (não `127.0.0.1`) de propósito: é o único formato para o
    /// qual o Microsoft identity platform ignora a porta ao comparar contra
    /// um redirect URI `http://localhost` cadastrado na plataforma "Mobile
    /// and desktop applications" do app registration. Qualquer variação
    /// (host diferente, porta fixa incompatível, caminho adicional) cai na
    /// comparação exata de string e falha com AADSTS50011.
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Aceita exatamente uma conexão HTTP, extrai `code`/`state` (ou
    /// `error`) da query string e responde com uma página de confirmação.
    /// Consome `self` — o listener não deve ser reutilizado após um login.
    pub async fn receive_authorization_code(
        self,
        expected_state: &str,
    ) -> Result<AuthorizationCode, AuthError> {
        let (mut stream, _) = self.listener.accept().await?;

        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let request_line = request.lines().next().unwrap_or_default();

        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default();
        let full_url = format!("http://127.0.0.1{path}");
        let parsed = url::Url::parse(&full_url).map_err(|_| AuthError::MissingAuthorizationCode)?;
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        let result = if let Some(error) = params.get("error") {
            let description = params
                .get("error_description")
                .cloned()
                .unwrap_or_default();
            Err(AuthError::AuthorizationDenied(format!(
                "{error}: {description}"
            )))
        } else {
            match (params.get("code"), params.get("state")) {
                (Some(code), Some(state)) if state == expected_state => Ok(AuthorizationCode {
                    code: code.clone(),
                    state: state.clone(),
                }),
                (Some(_), Some(_)) => Err(AuthError::StateMismatch),
                _ => Err(AuthError::MissingAuthorizationCode),
            }
        };

        let body = if result.is_ok() { SUCCESS_BODY } else { ERROR_BODY };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;

        result
    }
}
