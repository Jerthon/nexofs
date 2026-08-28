//! Armazenamento de refresh token no keyring do desktop (NFR-SEC-003).
//!
//! Usa a crate `keyring`, que fala Secret Service no GNOME e KWallet (via
//! Secret Service) no KDE — SQLite e logs nunca veem o valor do token.

use crate::error::AuthError;
use keyring::Entry;
use nexofs_domain::SecretToken;

/// `service` identifica o NexoFS perante o keyring (ex.: `"nexofs"`);
/// `account_key` identifica a conta dentro do NexoFS (ex.: o `AccountId`).
pub fn store_refresh_token(
    service: &str,
    account_key: &str,
    token: &SecretToken,
) -> Result<(), AuthError> {
    let entry = Entry::new(service, account_key)?;
    entry.set_password(token.expose())?;
    Ok(())
}

pub fn load_refresh_token(
    service: &str,
    account_key: &str,
) -> Result<Option<SecretToken>, AuthError> {
    let entry = Entry::new(service, account_key)?;
    match entry.get_password() {
        Ok(password) => Ok(Some(SecretToken::new(password))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(other) => Err(other.into()),
    }
}

/// Remoção completa ao desconectar a conta (NFR-SEC-007). Ausência prévia
/// da entrada não é um erro — o objetivo (nenhum segredo remanescente) já
/// está satisfeito.
pub fn delete_refresh_token(service: &str, account_key: &str) -> Result<(), AuthError> {
    let entry = Entry::new(service, account_key)?;
    match entry.delete_password() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(other) => Err(other.into()),
    }
}
