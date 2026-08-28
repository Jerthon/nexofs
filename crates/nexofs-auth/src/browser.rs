//! Abertura do navegador padrão do sistema (PRD §4.2, SPEC §22.1).

use crate::error::AuthError;

/// Usa `xdg-open` — presente por padrão em GNOME e KDE Plasma (PRD §13.1).
/// A URL é passada como argumento de processo, nunca interpolada em shell,
/// eliminando risco de command injection mesmo que a URL contenha
/// caracteres especiais.
pub fn open_system_browser(url: &str) -> Result<(), AuthError> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map_err(AuthError::BrowserLaunch)?;
    Ok(())
}
