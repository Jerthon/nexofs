use crate::filesystem::NexoFsFilesystem;
use fuser::{BackgroundSession, MountOption};
use nexofs_domain::{AccountId, ItemId, NamespaceId, ProviderId};
use nexofs_sync_core::SyncCore;
use std::path::Path;
use std::sync::Arc;

/// Monta o namespace em `mountpoint`, em uma thread dedicada gerenciada
/// internamente pelo `fuser` — retorna imediatamente. Desmontar acontece ao
/// dropar o `BackgroundSession` retornado (SPEC §8.1, FR-FS-001: sem daemon
/// privilegiado, montagem no contexto do próprio usuário).
pub fn mount(
    core: Arc<SyncCore>,
    root_item_id: ItemId,
    mountpoint: &Path,
    provider_id: ProviderId,
    account_id: AccountId,
    namespace_id: NamespaceId,
) -> std::io::Result<BackgroundSession> {
    let rt = tokio::runtime::Handle::current();
    let fs = NexoFsFilesystem::new(core, rt, root_item_id, provider_id, account_id, namespace_id);

    let options = [
        MountOption::FSName("nexofs".to_string()),
        MountOption::Subtype("nexofs".to_string()),
    ];

    fuser::spawn_mount2(fs, mountpoint, &options)
}
