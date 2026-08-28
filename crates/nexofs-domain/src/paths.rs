//! Resolução de diretórios XDG. SPEC §10.1, §11.1, §20.1.

use std::io;
use std::path::PathBuf;

/// Localizações persistentes e de runtime usadas pelo NexoFS, resolvidas a
/// partir do ambiente XDG com fallback documentado pela SPEC.
#[derive(Debug, Clone)]
pub struct NexoFsPaths {
    data_home: PathBuf,
    runtime_dir: Option<PathBuf>,
}

impl NexoFsPaths {
    /// Resolve a partir das variáveis de ambiente do processo atual.
    pub fn from_env() -> Self {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").expect(
                    "HOME deve estar definido em uma sessão Linux; NexoFS não é suportado sem ele",
                );
                PathBuf::from(home).join(".local/share")
            });

        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());

        Self {
            data_home,
            runtime_dir,
        }
    }

    fn root(&self) -> PathBuf {
        self.data_home.join("nexofs")
    }

    pub fn metadata_dir(&self) -> PathBuf {
        self.root().join("metadata")
    }

    pub fn sqlite_path(&self) -> PathBuf {
        self.metadata_dir().join("nexofs.sqlite3")
    }

    pub fn cache_clean_dir(&self) -> PathBuf {
        self.root().join("cache").join("clean")
    }

    pub fn cache_dirty_dir(&self) -> PathBuf {
        self.root().join("cache").join("dirty")
    }

    pub fn cache_partial_dir(&self) -> PathBuf {
        self.root().join("cache").join("partial")
    }

    pub fn cache_conflict_dir(&self) -> PathBuf {
        self.root().join("cache").join("conflict")
    }

    pub fn overlay_dir(&self, namespace_id: &str) -> PathBuf {
        self.root().join("overlay").join(namespace_id)
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.root().join("journal")
    }

    pub fn diagnostics_dir(&self) -> PathBuf {
        self.root().join("diagnostics")
    }

    /// `$XDG_RUNTIME_DIR/nexofs/control.sock` (SPEC §20.1). Retorna `None`
    /// quando `XDG_RUNTIME_DIR` não está definido — chamador decide o fallback.
    pub fn control_socket_path(&self) -> Option<PathBuf> {
        self.runtime_dir
            .as_ref()
            .map(|dir| dir.join("nexofs").join("control.sock"))
    }

    /// Cria toda a árvore de diretórios persistentes com permissão restrita
    /// ao usuário (`0700`), conforme NFR-SEC-003/NFR-SEC-006.
    pub fn ensure_data_dirs(&self) -> io::Result<()> {
        for dir in [
            self.metadata_dir(),
            self.cache_clean_dir(),
            self.cache_dirty_dir(),
            self.cache_partial_dir(),
            self.cache_conflict_dir(),
            self.root().join("overlay"),
            self.journal_dir(),
            self.diagnostics_dir(),
        ] {
            create_private_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Cria `$XDG_RUNTIME_DIR/nexofs/` com permissão restrita, se aplicável.
    pub fn ensure_runtime_dir(&self) -> io::Result<Option<PathBuf>> {
        let Some(socket_path) = self.control_socket_path() else {
            return Ok(None);
        };
        let dir = socket_path
            .parent()
            .expect("control_socket_path sempre tem diretório pai")
            .to_path_buf();
        create_private_dir_all(&dir)?;
        Ok(Some(dir))
    }
}

#[cfg(unix)]
fn create_private_dir_all(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    // `mode()` acima só se aplica ao componente final quando ele ainda não
    // existe; força a permissão também quando o diretório já existia.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir_all(path: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}
