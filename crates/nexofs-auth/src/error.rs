#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("erro de I/O no listener de loopback: {0}")]
    Io(#[from] std::io::Error),
    #[error("o provedor de identidade recusou a autorização: {0}")]
    AuthorizationDenied(String),
    #[error("resposta de redirecionamento sem parâmetro `code`")]
    MissingAuthorizationCode,
    #[error("o parâmetro `state` retornado não confere com o esperado — possível CSRF")]
    StateMismatch,
    #[error("erro no keyring do sistema: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("não foi possível abrir o navegador do sistema: {0}")]
    BrowserLaunch(std::io::Error),
}
