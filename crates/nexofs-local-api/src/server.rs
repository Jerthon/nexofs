use crate::state::AppState;
use axum::serve::Listener;
use std::path::Path;
use tokio::net::{UnixListener, UnixStream};

/// Sobe o servidor em `socket_path` (SPEC §20.1: `$XDG_RUNTIME_DIR/nexofs/control.sock`,
/// permissão `0600`). Bloqueia até o listener falhar — chamador deve rodar
/// isto em uma task própria (`tokio::spawn`).
///
/// SPEC §20.1: "o daemon DEVE validar UID do peer quando disponível"
/// (NFR-SEC-006/T5-01) — a permissão `0600` do arquivo por si só já
/// restringe o acesso ao mesmo usuário Unix, mas não impede outro processo
/// do MESMO usuário rodando com um UID efetivo diferente (setuid) de se
/// conectar; `SO_PEERCRED` fecha essa lacuna na camada de transporte, antes
/// de qualquer requisição HTTP ser processada.
pub async fn serve(socket_path: &Path, state: AppState) -> std::io::Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    }

    tracing::info!(path = %socket_path.display(), "API local escutando");
    let app = crate::routes::router(state);
    axum::serve(PeerUidValidatingListener { inner: listener, expected_uid: process_uid() }, app).await
}

fn process_uid() -> u32 {
    nix::unistd::Uid::current().as_raw()
}

/// Envolve `UnixListener` para descartar, antes mesmo do handshake HTTP,
/// qualquer conexão cujo UID de peer (via `SO_PEERCRED`) não seja o do
/// próprio processo do daemon — este socket é de um único usuário, nunca
/// multiusuário.
struct PeerUidValidatingListener {
    inner: UnixListener,
    expected_uid: u32,
}

impl Listener for PeerUidValidatingListener {
    type Io = UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = <UnixListener as Listener>::accept(&mut self.inner).await;
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == self.expected_uid => return (stream, addr),
                Ok(cred) => {
                    tracing::warn!(peer_uid = cred.uid(), expected_uid = self.expected_uid, "conexão recusada — UID do peer não corresponde ao dono do daemon");
                }
                Err(err) => {
                    tracing::warn!(%err, "conexão recusada — não foi possível obter SO_PEERCRED do peer");
                }
            }
            drop(stream);
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}
