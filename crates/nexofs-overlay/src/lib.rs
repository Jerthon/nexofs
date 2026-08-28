//! Local-Only Overlay (SPEC §11.4, T4-05): armazenamento persistente para
//! conteúdo que a avaliação de exclusão (`nexofs-ignore`) marcou como
//! `LocalOnly` — ao contrário de `nexofs-content-cache`, este conteúdo:
//!
//! - NÃO é cache: não corresponde a nenhum objeto remoto;
//! - NÃO é evictado por LRU nem por pressão de disco (T4-11/T4-14);
//! - nunca gera operação de journal enquanto a regra `LOCAL_ONLY` estiver
//!   ativa (SPEC §11.4) — quem decide isso é `nexofs-sync-core`, este crate
//!   só guarda os bytes.

use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("erro de I/O no overlay local-only: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct LocalOnlyOverlay {
    root: PathBuf,
}

impl LocalOnlyOverlay {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path_for(&self, cache_object_id: &str) -> PathBuf {
        self.root.join(cache_object_id)
    }

    pub fn exists(&self, cache_object_id: &str) -> bool {
        self.path_for(cache_object_id).is_file()
    }

    /// Cria o arquivo vazio deste item no overlay (idempotente — chamadas
    /// repetidas para o mesmo `cache_object_id` não truncam um arquivo já
    /// existente). Usado quando um arquivo novo é criado dentro de uma
    /// árvore `LocalOnly`.
    pub async fn create_empty(&self, cache_object_id: &str) -> Result<PathBuf, OverlayError> {
        tokio::fs::create_dir_all(&self.root).await?;
        let path = self.path_for(cache_object_id);
        if !path.is_file() {
            // T6-09/SPEC §22.4 ("symlink attack em cache"): `O_NOFOLLOW`
            // recusa abrir através de um symlink já existente — defesa em
            // profundidade contra outro processo do MESMO usuário (o
            // diretório já é privado) plantando um link num nome previsível
            // antes desta escrita.
            let file = tokio::fs::OpenOptions::new().write(true).create(true).truncate(true).custom_flags(libc::O_NOFOLLOW).open(&path).await?;
            AsyncWriteExt::flush(&mut { file }).await?;
        }
        Ok(path)
    }

    /// Remove o conteúdo deste item do overlay — usado quando o item é
    /// apagado localmente ou quando a exclusão que o mantinha `LocalOnly` é
    /// removida e o conteúdo migra para o fluxo normal (T4-08).
    /// Idempotente: remover algo que já não existe não é erro.
    pub fn remove(&self, cache_object_id: &str) -> Result<(), OverlayError> {
        match std::fs::remove_file(self.path_for(cache_object_id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// FR-LOC-002 ("participa do cálculo de uso local"): soma de bytes de
    /// todo conteúdo hoje persistido no overlay deste namespace.
    pub async fn total_bytes(&self) -> Result<u64, OverlayError> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(err.into()),
        };

        let mut total = 0u64;
        while let Some(entry) = entries.next_entry().await? {
            if let Ok(metadata) = entry.metadata().await {
                if metadata.is_file() {
                    total += metadata.len();
                }
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay_at(dir: &tempfile::TempDir) -> LocalOnlyOverlay {
        LocalOnlyOverlay::new(dir.path().join("overlay"))
    }

    #[tokio::test]
    async fn create_empty_materializes_a_file_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = overlay_at(&dir);
        assert!(!overlay.exists("a"));

        let path1 = overlay.create_empty("a").await.unwrap();
        assert!(overlay.exists("a"));

        tokio::fs::write(&path1, b"conteudo local-only").await.unwrap();
        let path2 = overlay.create_empty("a").await.unwrap();
        assert_eq!(path1, path2);
        assert_eq!(tokio::fs::read(&path2).await.unwrap(), b"conteudo local-only", "chamada repetida não pode truncar conteúdo existente");
    }

    #[tokio::test]
    async fn remove_is_a_no_op_when_the_file_never_existed() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = overlay_at(&dir);
        overlay.remove("nunca-existiu").unwrap();
    }

    #[tokio::test]
    async fn total_bytes_sums_every_file_and_is_zero_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = overlay_at(&dir);
        assert_eq!(overlay.total_bytes().await.unwrap(), 0);

        let path_a = overlay.create_empty("a").await.unwrap();
        tokio::fs::write(&path_a, b"12345").await.unwrap();
        let path_b = overlay.create_empty("b").await.unwrap();
        tokio::fs::write(&path_b, b"1234567890").await.unwrap();

        assert_eq!(overlay.total_bytes().await.unwrap(), 15);

        overlay.remove("a").unwrap();
        assert_eq!(overlay.total_bytes().await.unwrap(), 10);
    }
}
