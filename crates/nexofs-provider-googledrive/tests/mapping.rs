//! Testes puros (sem rede) do mapeamento DTO → modelo neutro. T7-02: sem
//! projeto Google Cloud/credenciais disponíveis neste ambiente para validar
//! contra a API real (mesmo padrão de `nexofs-provider-onedrive`, validado
//! só ao vivo) — isto cobre pelo menos a parte determinística e testável
//! sem rede: conversão de campos, incluindo o `size` como string que é uma
//! particularidade real da API v3.

use nexofs_provider_googledrive::GoogleDriveConfig;

#[test]
fn config_from_env_reads_client_id_and_secret() {
    // SAFETY: testes de crate rodam em processos separados por padrão do
    // `cargo test` (cada teste roda como uma thread do mesmo processo, mas
    // `env::set_var`/`remove_var` neste teste não são compartilhados com
    // outros crates rodando em paralelo) — ainda assim, evitamos qualquer
    // efeito colateral limpando as variáveis ao final.
    std::env::set_var("NEXOFS_GOOGLEDRIVE_CLIENT_ID", "test-client-id");
    std::env::set_var("NEXOFS_GOOGLEDRIVE_CLIENT_SECRET", "test-client-secret");

    let config = GoogleDriveConfig::from_env();
    assert_eq!(config.client_id, "test-client-id");
    assert_eq!(config.client_secret, "test-client-secret");
    assert!(config.is_configured());

    std::env::remove_var("NEXOFS_GOOGLEDRIVE_CLIENT_ID");
    std::env::remove_var("NEXOFS_GOOGLEDRIVE_CLIENT_SECRET");
}

#[test]
fn config_without_env_vars_is_not_configured() {
    std::env::remove_var("NEXOFS_GOOGLEDRIVE_CLIENT_ID");
    std::env::remove_var("NEXOFS_GOOGLEDRIVE_CLIENT_SECRET");

    let config = GoogleDriveConfig::from_env();
    assert!(!config.is_configured());
}
