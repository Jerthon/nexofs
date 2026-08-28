//! Alocação de inode estável por identidade remota. SPEC §8.3.

use crate::ids::{AccountId, Inode, NamespaceId, ProviderId};
use std::hash::{Hash, Hasher};

/// Deriva um inode a partir da identidade remota (ou UUID local), nunca do
/// caminho — rename/move não pode invalidar o inode (FR-FS-004).
///
/// Colisões de hash devem ser detectadas e resolvidas via `inode_map`
/// persistente (SPEC §10.3, tabela `inode_map`); esta função não garante
/// unicidade absoluta por si só.
pub fn stable_inode(
    provider_id: &ProviderId,
    account_id: &AccountId,
    namespace_id: &NamespaceId,
    remote_or_local_key: &str,
) -> Inode {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    provider_id.hash(&mut hasher);
    account_id.hash(&mut hasher);
    namespace_id.hash(&mut hasher);
    remote_or_local_key.hash(&mut hasher);
    // FUSE reserva o inode 1 para a raiz; evita colisão com valores baixos.
    let raw = hasher.finish();
    Inode(raw.max(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_identity_yields_same_inode() {
        let provider = ProviderId::from("onedrive");
        let account = AccountId::new();
        let namespace = NamespaceId::new();

        let a = stable_inode(&provider, &account, &namespace, "remote-item-1");
        let b = stable_inode(&provider, &account, &namespace, "remote-item-1");
        assert_eq!(a, b);
    }

    #[test]
    fn different_identity_usually_differs() {
        let provider = ProviderId::from("onedrive");
        let account = AccountId::new();
        let namespace = NamespaceId::new();

        let a = stable_inode(&provider, &account, &namespace, "remote-item-1");
        let b = stable_inode(&provider, &account, &namespace, "remote-item-2");
        assert_ne!(a, b);
    }

    #[test]
    fn rename_does_not_change_inode() {
        // O inode depende da identidade remota, não do caminho — portanto
        // esta função nunca recebe o caminho como entrada.
        let provider = ProviderId::from("onedrive");
        let account = AccountId::new();
        let namespace = NamespaceId::new();

        let before_rename = stable_inode(&provider, &account, &namespace, "remote-item-1");
        // Simula rename: a chave de identidade (remote_item_id) não muda.
        let after_rename = stable_inode(&provider, &account, &namespace, "remote-item-1");
        assert_eq!(before_rename, after_rename);
    }
}
