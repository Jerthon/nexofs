//! Identificadores fortemente tipados. SPEC §4.1.
//!
//! Nunca usar `String`/`u64` cru para identidade em assinaturas públicas —
//! a distinção de tipo evita trocar, por exemplo, um `AccountId` por um
//! `NamespaceId` no ponto de chamada.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub uuid::Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl From<uuid::Uuid> for $name {
            fn from(value: uuid::Uuid) -> Self {
                Self(value)
            }
        }
    };
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

// Identificador estável do tipo de provedor (ex.: "onedrive", "googledrive").
// É texto, não UUID, porque é conhecido em tempo de compilação por adaptador.
string_id!(ProviderId);

uuid_id!(AccountId);
uuid_id!(NamespaceId);
uuid_id!(ItemId);
uuid_id!(OperationId);
uuid_id!(ConflictId);

// Identificador emitido pelo provedor remoto — opaco, nunca gerado localmente.
string_id!(RemoteItemId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Inode(pub u64);

impl fmt::Display for Inode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
