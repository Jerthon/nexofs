//! Configuração do adaptador OneDrive. SPEC §6.

/// Qual autoridade do Microsoft identity platform usar no login.
///
/// `client_id` não é segredo — é o identificador público do aplicativo
/// NexoFS perante a Microsoft, do mesmo jeito que o rclone e outros
/// clientes de terceiros embutem o próprio `client_id` no binário
/// (NFR-SEC-002: a ausência de *client secret*, não de *client id*, é o
/// requisito de segurança).
#[derive(Debug, Clone)]
pub enum TenantHint {
    /// Contas pessoais e corporativas de qualquer organização (FR-ACC-001 +
    /// FR-ACC-002 no mesmo login). Exige que o app registration esteja
    /// configurado no Azure Portal como "Contas em qualquer diretório
    /// organizacional e contas pessoais da Microsoft" — caso contrário a
    /// Microsoft recusa a autorização com AADSTS900023/50020 antes mesmo de
    /// chegar ao redirect. Padrão do NexoFS.
    Common,
    /// Um tenant específico — só aceita contas que pertencem a ele. Útil
    /// para instalações corporativas que deliberadamente restringem o login
    /// a um único diretório.
    Specific(String),
}

impl TenantHint {
    pub fn as_path_segment(&self) -> &str {
        match self {
            TenantHint::Common => "common",
            TenantHint::Specific(tenant_id) => tenant_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OneDriveConfig {
    pub client_id: String,
    pub tenant: TenantHint,
    pub scopes: Vec<String>,
}

impl OneDriveConfig {
    /// Lê `NEXOFS_ONEDRIVE_CLIENT_ID`/`NEXOFS_ONEDRIVE_TENANT_ID` do
    /// ambiente, com fallback para `default_client_id` e para `common`
    /// (client_id e tenant não são segredos — ver doc de `TenantHint`).
    /// `NEXOFS_ONEDRIVE_TENANT_ID=common`/`organizations`/`consumers` são
    /// aceitos como autoridades especiais; qualquer outro valor é tratado
    /// como um tenant específico.
    pub fn from_env_or_defaults(default_client_id: &str) -> Self {
        let client_id =
            std::env::var("NEXOFS_ONEDRIVE_CLIENT_ID").unwrap_or_else(|_| default_client_id.to_string());
        let tenant = match std::env::var("NEXOFS_ONEDRIVE_TENANT_ID") {
            Ok(value) if value.eq_ignore_ascii_case("common") => TenantHint::Common,
            Ok(value) => TenantHint::Specific(value),
            Err(_) => TenantHint::Common,
        };

        Self {
            client_id,
            tenant,
            scopes: vec![
                "offline_access".to_string(),
                "Files.ReadWrite".to_string(),
                "User.Read".to_string(),
            ],
        }
    }

    pub fn authorize_endpoint(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize",
            self.tenant.as_path_segment()
        )
    }

    pub fn token_endpoint(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant.as_path_segment()
        )
    }
}
