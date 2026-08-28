//! Filesystem virtual FUSE — traduz o protocolo POSIX/FUSE para o
//! `SyncCore` genérico. Não chama adaptadores de provedor diretamente
//! (SPEC §3.2).

mod activity;
mod filesystem;
mod mount;

pub use activity::ActivityPolicy;

pub use fuser::BackgroundSession;
pub use mount::mount;
