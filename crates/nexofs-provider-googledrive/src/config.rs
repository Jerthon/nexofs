//! Configuração do adaptador Google Drive. SPEC §5 (Fase 7, T7-02).
//!
//! O `client_id`/`client_secret` pertencem ao *app NexoFS*, não a cada
//! usuário final — igual ao app registration da Microsoft usado pelo
//! OneDrive (ADR-013), é um único OAuth client "Desktop" registrado uma vez
//! pelo projeto e embutido no binário distribuído pelo instalador. O
//! usuário final nunca configura nada: só autentica com a própria conta
//! Google no fluxo de consentimento.
//!
//! Isso é seguro porque o Google trata o `client_secret` de um OAuth client
//! tipo "Desktop app" como não confidencial (RFC 8252 — clientes nativos
//! não conseguem manter segredo mesmo embutido em texto; o Google documenta
//! isso explicitamente). É diferente de um secret de servidor.
//!
//! O embutimento é em tempo de compilação via `option_env!`, lido do
//! ambiente de build (CI define `NEXOFS_GOOGLEDRIVE_CLIENT_ID`/`_SECRET`
//! antes de `cargo build --release`). Uma variável de ambiente em tempo de
//! execução com o mesmo nome ainda funciona como *override* — para
//! instalações que preferem usar seu próprio projeto Google Cloud (ex:
//! empresa com cota própria), sem precisar recompilar.

const EMBEDDED_CLIENT_ID: Option<&str> = option_env!("NEXOFS_GOOGLEDRIVE_CLIENT_ID");
const EMBEDDED_CLIENT_SECRET: Option<&str> = option_env!("NEXOFS_GOOGLEDRIVE_CLIENT_SECRET");

#[derive(Debug, Clone, Default)]
pub struct GoogleDriveConfig {
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
}

impl GoogleDriveConfig {
    /// Resolve `client_id`/`client_secret` na ordem: variável de ambiente em
    /// runtime (override) > valor embutido em tempo de compilação (o app
    /// NexoFS) > vazio. Nunca `panic!` quando ausentes, para o processo
    /// continuar subindo mesmo sem Google Drive configurado (só a
    /// tentativa de login com esta conta falharia, não o daemon inteiro).
    pub fn from_env() -> Self {
        Self {
            client_id: resolve("NEXOFS_GOOGLEDRIVE_CLIENT_ID", EMBEDDED_CLIENT_ID),
            client_secret: resolve("NEXOFS_GOOGLEDRIVE_CLIENT_SECRET", EMBEDDED_CLIENT_SECRET),
            scopes: vec![
                "https://www.googleapis.com/auth/drive".to_string(),
                "https://www.googleapis.com/auth/userinfo.profile".to_string(),
                "https://www.googleapis.com/auth/userinfo.email".to_string(),
            ],
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }

    pub fn authorize_endpoint(&self) -> &'static str {
        "https://accounts.google.com/o/oauth2/v2/auth"
    }

    pub fn token_endpoint(&self) -> &'static str {
        "https://oauth2.googleapis.com/token"
    }
}

fn resolve(env_var: &str, embedded: Option<&'static str>) -> String {
    std::env::var(env_var).ok().filter(|value| !value.is_empty()).or_else(|| embedded.map(String::from)).unwrap_or_default()
}
