//! Reexportado de `nexofs-domain` — mantido aqui para não quebrar
//! `nexofs_provider_api::SecretToken` nos crates que já importam por este
//! caminho. `SecretToken` é um tipo de infraestrutura genérico (usado
//! também por `nexofs-auth`), não específico de provedor.

pub use nexofs_domain::SecretToken;
