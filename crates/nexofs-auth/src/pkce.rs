//! PKCE (RFC 7636) — obrigatório junto de Authorization Code (NFR-SEC-001).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nexofs_domain::SecretToken;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// `code_verifier` gerado localmente e nunca transmitido até a troca do
/// código por token — impede que um código interceptado (ex.: por outro
/// processo local lendo o redirect) seja trocado por um invasor sem também
/// conhecer o verifier original.
pub struct PkceVerifier(SecretToken);

impl PkceVerifier {
    /// 32 bytes aleatórios em base64url sem padding = 43 caracteres,
    /// dentro da faixa exigida pela RFC (43–128).
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(SecretToken::new(URL_SAFE_NO_PAD.encode(bytes)))
    }

    pub fn verifier(&self) -> &SecretToken {
        &self.0
    }

    /// `code_challenge` (método S256) derivado do verifier — este valor,
    /// diferente do verifier, é seguro para incluir na URL de autorização.
    pub fn challenge_s256(&self) -> String {
        let digest = Sha256::digest(self.0.expose().as_bytes());
        URL_SAFE_NO_PAD.encode(digest)
    }
}

/// Token opaco anti-CSRF do fluxo OAuth (parâmetro `state`).
pub fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_deterministic_from_verifier() {
        let verifier = PkceVerifier::generate();
        assert_eq!(verifier.challenge_s256(), verifier.challenge_s256());
    }

    #[test]
    fn verifier_has_valid_length_for_rfc7636() {
        let verifier = PkceVerifier::generate();
        let len = verifier.verifier().expose().len();
        assert!((43..=128).contains(&len), "len={len}");
    }

    #[test]
    fn state_values_are_not_repeated() {
        assert_ne!(generate_state(), generate_state());
    }
}
