//! Remote Content Cache: hidratação sob demanda com arquivo temporário,
//! validação de integridade e promoção atômica (FR-HYD-002, SPEC §11.2).
//!
//! `clean/` guarda conteúdo íntegro e elegível a eviction (política LRU real
//! chega na Fase 2, T2-13). `partial/` nunca é exposto a um leitor — um
//! download interrompido deixa, no máximo, um arquivo órfão em `partial/`,
//! nunca um arquivo incompleto em `clean/`.

mod disk_pressure;

pub use disk_pressure::{disk_pressure_level, DiskPressureLevel};

use nexofs_provider_api::DownloadHandle;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

/// T6-09/SPEC §22.4 ("symlink attack em cache"): recusa abrir através de um
/// symlink já existente no caminho — os diretórios de cache são privados
/// (`0700`, só o próprio usuário do daemon), então isto é defesa em
/// profundidade contra outro processo rodando como o MESMO usuário
/// plantando um link simbólico num nome previsível antes do NexoFS escrever
/// ali (ex.: redirecionando a escrita para sobrescrever um arquivo
/// arbitrário que esse usuário possa escrever) — não contra outro usuário
/// do sistema, que os diretórios `0700` já bloqueiam por si só.
async fn create_new_regular_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new().write(true).create(true).truncate(true).custom_flags(libc::O_NOFOLLOW).open(path).await
}

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("erro de I/O no cache de conteúdo: {0}")]
    Io(#[from] std::io::Error),
    #[error("tamanho recebido ({received}) difere do content_length anunciado ({expected})")]
    SizeMismatch { expected: u64, received: u64 },
}

#[derive(Clone)]
pub struct ContentCache {
    clean_dir: PathBuf,
    partial_dir: PathBuf,
    dirty_dir: PathBuf,
}

impl ContentCache {
    pub fn new(clean_dir: PathBuf, partial_dir: PathBuf, dirty_dir: PathBuf) -> Self {
        Self { clean_dir, partial_dir, dirty_dir }
    }

    pub fn clean_path(&self, cache_object_id: &str) -> PathBuf {
        self.clean_dir.join(cache_object_id)
    }

    /// T4-14: diretório usado para consultar a pressão de disco real
    /// (`disk_pressure_level`) — `clean/`, `dirty/` e `partial/` vivem no
    /// mesmo sistema de arquivos na prática, então qualquer um serve.
    pub fn root_dir(&self) -> &Path {
        &self.clean_dir
    }

    pub fn dirty_path(&self, cache_object_id: &str) -> PathBuf {
        self.dirty_dir.join(cache_object_id)
    }

    pub fn is_hydrated(&self, cache_object_id: &str) -> bool {
        self.clean_path(cache_object_id).is_file()
    }

    pub fn has_dirty(&self, cache_object_id: &str) -> bool {
        self.dirty_path(cache_object_id).is_file()
    }

    /// SPEC §16.1: primeira escrita sobre um item `Clean` materializa uma
    /// cópia local dirty — reflink quando o sistema de arquivos suportar
    /// (COW real, ex.: Btrfs/XFS), cópia física caso contrário
    /// (`reflink_or_copy` decide isso sozinho). Nunca modifica `clean/`, que
    /// continua correspondendo à versão remota registrada até um upload
    /// confirmar uma nova. Idempotente: chamadas repetidas para o mesmo
    /// `cache_object_id` (ex.: cada `write()` do FUSE) reaproveitam a cópia
    /// já materializada em vez de recriá-la.
    pub async fn begin_dirty_write(
        &self,
        cache_object_id: &str,
        base_clean: Option<&Path>,
    ) -> Result<PathBuf, CacheError> {
        tokio::fs::create_dir_all(&self.dirty_dir).await?;
        let dirty_path = self.dirty_path(cache_object_id);
        if dirty_path.is_file() {
            return Ok(dirty_path);
        }

        match base_clean {
            Some(base) if base.is_file() => {
                let base = base.to_path_buf();
                let target = dirty_path.clone();
                tokio::task::spawn_blocking(move || reflink_copy::reflink_or_copy(&base, &target))
                    .await
                    .expect("spawn_blocking não deve entrar em pânico")?;
            }
            _ => {
                create_new_regular_file(&dirty_path).await?;
            }
        }
        Ok(dirty_path)
    }

    /// Promove o conteúdo dirty a `clean/` após o upload confirmar a nova
    /// versão remota (SPEC §16.3) — a partir daqui o objeto volta a ser
    /// elegível a eviction como qualquer conteúdo hidratado normal.
    pub async fn promote_dirty_to_clean(&self, cache_object_id: &str) -> Result<PathBuf, CacheError> {
        tokio::fs::create_dir_all(&self.clean_dir).await?;
        let dirty_path = self.dirty_path(cache_object_id);
        let clean_path = self.clean_path(cache_object_id);
        tokio::fs::rename(&dirty_path, &clean_path).await?;
        Ok(clean_path)
    }

    /// Descarta a cópia dirty sem promovê-la — usado quando um item criado e
    /// apagado localmente antes de qualquer upload é coalescido a nada
    /// (SPEC §13.4).
    pub fn remove_dirty(&self, cache_object_id: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.dirty_path(cache_object_id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn upload_snapshot_path(&self, cache_object_id: &str) -> PathBuf {
        self.partial_dir.join(format!("{cache_object_id}.uploading"))
    }

    /// Congela o conteúdo dirty atual antes de um upload (reflink quando
    /// suportado) — o dispatcher lê desta cópia, nunca do arquivo dirty
    /// "ao vivo", que o FUSE pode seguir escrevendo (uma nova escrita
    /// durante um upload em andamento não pode corromper os bytes já em
    /// trânsito). A geração congelada aqui é exatamente a que o
    /// `idempotency_key` da operação (que embute `local_version`)
    /// referencia.
    pub async fn snapshot_dirty_for_upload(&self, cache_object_id: &str) -> Result<PathBuf, CacheError> {
        tokio::fs::create_dir_all(&self.partial_dir).await?;
        let dirty_path = self.dirty_path(cache_object_id);
        let snapshot_path = self.upload_snapshot_path(cache_object_id);
        tokio::task::spawn_blocking(move || reflink_copy::reflink_or_copy(&dirty_path, &snapshot_path))
            .await
            .expect("spawn_blocking não deve entrar em pânico")?;
        Ok(self.upload_snapshot_path(cache_object_id))
    }

    pub fn remove_upload_snapshot(&self, cache_object_id: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.upload_snapshot_path(cache_object_id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Remove um objeto elegível do cache (FR-CACHE-002, eviction LRU). A
    /// verificação de elegibilidade — pinned/dirty/conflito/aberto — é
    /// responsabilidade de quem chama (`SyncCore`, que tem acesso ao
    /// índice); este método só executa a remoção física.
    pub fn remove(&self, cache_object_id: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.clean_path(cache_object_id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Baixa `download` inteiro para um arquivo temporário em `partial/`,
    /// confere o tamanho contra `content_length` (quando anunciado) e só
    /// então promove por `rename` atômico para `clean/`. Em qualquer falha
    /// no meio do caminho, o arquivo parcial é removido — nunca fica
    /// visível como se fosse conteúdo íntegro (FR-HYD-002).
    pub async fn hydrate(
        &self,
        cache_object_id: &str,
        mut download: DownloadHandle,
    ) -> Result<PathBuf, CacheError> {
        tokio::fs::create_dir_all(&self.partial_dir).await?;
        tokio::fs::create_dir_all(&self.clean_dir).await?;

        let partial_path = self.partial_dir.join(format!("{cache_object_id}.part"));
        let expected_len = download.content_length;

        let result = self
            .write_and_validate(&partial_path, &mut download, expected_len)
            .await;

        match result {
            Ok(written) => {
                let clean_path = self.clean_path(cache_object_id);
                tokio::fs::rename(&partial_path, &clean_path).await?;
                tracing::debug!(cache_object_id, written, "conteúdo hidratado e promovido");
                Ok(clean_path)
            }
            Err(err) => {
                let _ = tokio::fs::remove_file(&partial_path).await;
                Err(err)
            }
        }
    }

    async fn write_and_validate(
        &self,
        partial_path: &Path,
        download: &mut DownloadHandle,
        expected_len: Option<u64>,
    ) -> Result<u64, CacheError> {
        let mut file = create_new_regular_file(partial_path).await?;
        let written = tokio::io::copy(&mut download.reader, &mut file).await?;
        file.flush().await?;
        file.sync_all().await?;

        if let Some(expected) = expected_len {
            if expected != written {
                return Err(CacheError::SizeMismatch {
                    expected,
                    received: written,
                });
            }
        }

        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use tokio::io::AsyncRead;

    fn in_memory_download(data: &'static [u8], announced_len: Option<u64>) -> DownloadHandle {
        DownloadHandle {
            reader: Box::pin(data) as Pin<Box<dyn AsyncRead + Send>>,
            content_length: announced_len,
            remote_content_version: None,
        }
    }

    #[tokio::test]
    async fn hydrate_promotes_to_clean_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::new(dir.path().join("clean"), dir.path().join("partial"), dir.path().join("dirty"));

        let path = cache
            .hydrate("item-1", in_memory_download(b"conteudo", Some(8)))
            .await
            .unwrap();

        assert!(path.starts_with(dir.path().join("clean")));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"conteudo");
        assert!(cache.is_hydrated("item-1"));
    }

    #[tokio::test]
    async fn hydrate_rejects_size_mismatch_and_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::new(dir.path().join("clean"), dir.path().join("partial"), dir.path().join("dirty"));

        let err = cache
            .hydrate("item-2", in_memory_download(b"conteudo", Some(999)))
            .await
            .unwrap_err();

        assert!(matches!(err, CacheError::SizeMismatch { .. }));
        assert!(!cache.is_hydrated("item-2"));
        assert!(!dir.path().join("partial").join("item-2.part").exists());
    }

    /// T6-09/SPEC §22.4 ("symlink attack em cache"): se outro processo do
    /// mesmo usuário já colocou um link simbólico no caminho onde o NexoFS
    /// está prestes a escrever (ex.: apontando para um arquivo arbitrário
    /// que esse usuário pode escrever), a hidratação deve recusar em vez de
    /// seguir o link e escrever no alvo dele.
    #[tokio::test]
    async fn hydrate_refuses_to_follow_a_preexisting_symlink_at_the_partial_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::new(dir.path().join("clean"), dir.path().join("partial"), dir.path().join("dirty"));
        tokio::fs::create_dir_all(dir.path().join("partial")).await.unwrap();

        let attack_target = dir.path().join("arquivo-de-outra-pessoa.txt");
        tokio::fs::write(&attack_target, b"conteudo original, nao deveria ser sobrescrito").await.unwrap();
        std::os::unix::fs::symlink(&attack_target, dir.path().join("partial").join("item-3.part")).unwrap();

        let err = cache.hydrate("item-3", in_memory_download(b"conteudo malicioso", Some(18))).await.unwrap_err();
        assert!(matches!(err, CacheError::Io(_)), "esperava um erro de I/O (ELOOP via O_NOFOLLOW), recebi: {err:?}");
        assert_eq!(
            tokio::fs::read(&attack_target).await.unwrap(),
            b"conteudo original, nao deveria ser sobrescrito",
            "o arquivo apontado pelo symlink nunca deveria ter sido tocado"
        );
    }
}
