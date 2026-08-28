//! Utilitários de autenticação reutilizáveis por qualquer adaptador OAuth:
//! PKCE, redirect loopback local e keyring do desktop. Nenhum tipo aqui é
//! específico de um provedor — as URLs de autorização/token e o parsing da
//! resposta pertencem ao adaptador (ex.: `nexofs-provider-onedrive`).

mod browser;
mod error;
mod keyring_store;
mod loopback;
mod pkce;

pub use browser::open_system_browser;
pub use error::AuthError;
pub use keyring_store::{delete_refresh_token, load_refresh_token, store_refresh_token};
pub use loopback::{AuthorizationCode, LoopbackListener};
pub use pkce::{generate_state, PkceVerifier};
