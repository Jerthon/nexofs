//! Adaptação do protocolo FUSE (via `fuser`) para o `SyncCore` genérico.
//! Cobre leitura (`lookup`, `getattr`, `opendir`, `readdir`, `open`, `read`)
//! e escrita (`create`, `write`, `flush`, `fsync`, `release`, `mkdir`,
//! `rename`, `unlink`, `rmdir`, `setattr`/truncate) — SPEC §8.2, §16.

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use libc::{EIO, EISDIR, ENOENT};
use nexofs_domain::inode::stable_inode;
use nexofs_domain::{AccountId, ItemId, NamespaceId, ProviderId};
use nexofs_provider_api::{ItemKind, ProviderErrorKind};
use nexofs_sync_core::{IndexedItem, SyncCore, SyncError};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ATTR_TTL: Duration = Duration::from_secs(5);
const ROOT_INODE: u64 = 1;

pub struct NexoFsFilesystem {
    core: Arc<SyncCore>,
    rt: tokio::runtime::Handle,
    root_item_id: ItemId,
    provider_id: ProviderId,
    account_id: AccountId,
    namespace_id: NamespaceId,
    uid: u32,
    gid: u32,
    inode_to_item: Mutex<HashMap<u64, ItemId>>,
    open_files: Mutex<HashMap<u64, OpenFile>>,
    /// T6-02 (`PRD §15.2`, "diretório com 100 mil filhos sem carregar todos
    /// em memória repetidamente"): o kernel FUSE drena um diretório grande
    /// através de várias chamadas a `readdir` em sequência (cada uma
    /// preenchendo o que couber no buffer dele, retomando do `offset`
    /// devolvido pela anterior) — sem cachear por `fh`, cada uma dessas
    /// chamadas re-executava `list_children` inteiro do zero (recarregando
    /// os 100 mil itens de novo só para usar uma fatia pequena), um custo
    /// O(n²/tamanho_do_buffer) para listar um diretório só uma vez. Agora a
    /// consulta roda uma única vez por sessão de `opendir`/`releasedir`.
    open_dirs: Mutex<HashMap<u64, Vec<(u64, FileType, String)>>>,
    next_fh: AtomicU64,
}

struct OpenFile {
    item_id: ItemId,
    file: std::fs::File,
    writable: bool,
}

impl NexoFsFilesystem {
    pub fn new(
        core: Arc<SyncCore>,
        rt: tokio::runtime::Handle,
        root_item_id: ItemId,
        provider_id: ProviderId,
        account_id: AccountId,
        namespace_id: NamespaceId,
    ) -> Self {
        let mut inode_to_item = HashMap::new();
        inode_to_item.insert(ROOT_INODE, root_item_id);

        Self {
            core,
            rt,
            root_item_id,
            provider_id,
            account_id,
            namespace_id,
            // SAFETY/contexto: o daemon roda como o próprio usuário (FR-FS-001,
            // "sem daemon privilegiado") — uid/gid do processo é sempre o do
            // dono da montagem.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            inode_to_item: Mutex::new(inode_to_item),
            open_files: Mutex::new(HashMap::new()),
            open_dirs: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    fn resolve_inode(&self, ino: u64) -> Option<ItemId> {
        self.inode_to_item.lock().expect("lock síncrono").get(&ino).copied()
    }

    /// Deriva o inode a partir da identidade remota (ADR-011) — estável
    /// entre chamadas, exceto para a raiz sintética, sempre `1`.
    fn inode_for(&self, item: &IndexedItem) -> u64 {
        if item.item_id == self.root_item_id {
            return ROOT_INODE;
        }
        let key = item
            .remote_item_id
            .clone()
            .unwrap_or_else(|| item.item_id.to_string());
        stable_inode(&self.provider_id, &self.account_id, &self.namespace_id, &key).0
    }

    fn register_inode(&self, inode: u64, item_id: ItemId) {
        self.inode_to_item.lock().expect("lock síncrono").insert(inode, item_id);
    }

    fn file_attr(&self, inode: u64, item: &IndexedItem) -> FileAttr {
        let kind = match item.kind {
            ItemKind::Directory => FileType::Directory,
            ItemKind::File => FileType::RegularFile,
        };
        let perm: u16 = match kind {
            FileType::Directory => 0o755,
            _ => 0o644,
        };
        let mtime = item
            .remote_modified_at_unix
            .filter(|t| *t >= 0)
            .map(|t| UNIX_EPOCH + Duration::from_secs(t as u64))
            .unwrap_or(UNIX_EPOCH);

        FileAttr {
            ino: inode,
            size: item.size_bytes,
            blocks: item.size_bytes.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    fn errno_for(err: &SyncError) -> i32 {
        match err {
            SyncError::NotFound => ENOENT,
            SyncError::AlreadyExists => libc::EEXIST,
            SyncError::NotEmpty => libc::ENOTEMPTY,
            SyncError::NotADirectory => libc::ENOTDIR,
            SyncError::IsADirectory => EISDIR,
            // FR-OFF-004: um placeholder sem rede para hidratar não pode
            // virar `EIO` — a aplicação (e o usuário) leria isso como disco/
            // arquivo corrompido, quando na verdade é só falta de
            // conectividade. `ENETUNREACH` é o mesmo sinal que sistemas de
            // arquivo de rede (NFS, sshfs) já usam para essa distinção.
            SyncError::Provider(provider_err) if matches!(provider_err.kind, ProviderErrorKind::Network | ProviderErrorKind::Timeout) => {
                libc::ENETUNREACH
            }
            _ => EIO,
        }
    }

    /// Corpo comum de `unlink`/`rmdir` — só muda o `ItemKind` esperado, que
    /// decide se um alvo do tipo errado vira `ENOTDIR` ou `EISDIR` em vez de
    /// apagar o que não devia.
    fn remove_item(&mut self, parent: u64, name: &OsStr, kind: ItemKind, reply: ReplyEmpty) {
        let Some(parent_item_id) = self.resolve_inode(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(name_str) = name.to_str() else {
            reply.error(ENOENT);
            return;
        };

        match self.rt.block_on(self.core.delete_local_item(parent_item_id, name_str, kind)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(Self::errno_for(&err)),
        }
    }
}

impl Filesystem for NexoFsFilesystem {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_item_id) = self.resolve_inode(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(ENOENT);
            return;
        };

        match self.rt.block_on(self.core.lookup_child(parent_item_id, name)) {
            Ok(Some(item)) => {
                let inode = self.inode_for(&item);
                self.register_inode(inode, item.item_id);
                reply.entry(&ATTR_TTL, &self.file_attr(inode, &item), 0);
            }
            Ok(None) => reply.error(ENOENT),
            Err(err) => {
                tracing::warn!(?err, name, "lookup falhou");
                reply.error(Self::errno_for(&err));
            }
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let Some(item_id) = self.resolve_inode(ino) else {
            reply.error(ENOENT);
            return;
        };

        match self.rt.block_on(self.core.get_item(item_id)) {
            Ok(Some(item)) => reply.attr(&ATTR_TTL, &self.file_attr(ino, &item)),
            Ok(None) => reply.error(ENOENT),
            Err(err) => reply.error(Self::errno_for(&err)),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(item_id) = self.resolve_inode(ino) else {
            reply.error(ENOENT);
            return;
        };

        // SPEC §16.1: `setattr` com `size` definido é a outra porta de
        // entrada do copy-on-write, além de `write` — cobre `truncate(2)` e
        // o `O_TRUNC` de `open(2)`. Outros atributos (mode/uid/gid/times)
        // não são persistidos no MVP (mount de usuário único, sem daemon
        // privilegiado) — a chamada só não falha, para não quebrar
        // ferramentas que os definem incidentalmente (ex.: `cp -p`, `touch`).
        if let Some(size) = size {
            if let Err(err) = self.rt.block_on(self.core.truncate_local(item_id, size)) {
                reply.error(Self::errno_for(&err));
                return;
            }
        }

        match self.rt.block_on(self.core.get_item(item_id)) {
            Ok(Some(item)) => reply.attr(&ATTR_TTL, &self.file_attr(ino, &item)),
            Ok(None) => reply.error(ENOENT),
            Err(err) => reply.error(Self::errno_for(&err)),
        }
    }

    fn opendir(&mut self, req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        if let Some(item_id) = self.resolve_inode(ino) {
            let interactive = crate::activity::is_interactive(req.pid(), crate::activity::cached_policy());
            self.core.mark_directory_active(item_id, interactive);
        }
        // T6-02: aloca um `fh` de verdade (em vez do `0` fixo de antes) para
        // `readdir` poder cachear a listagem por sessão de abertura — `0`
        // compartilhado por toda e qualquer pasta aberta ao mesmo tempo não
        // dava numa chave utilizável para isso.
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        reply.opened(fh, 0);
    }

    fn releasedir(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _flags: i32, reply: ReplyEmpty) {
        self.open_dirs.lock().expect("lock síncrono").remove(&fh);
        reply.ok();
    }

    fn readdir(&mut self, req: &Request<'_>, ino: u64, fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let Some(item_id) = self.resolve_inode(ino) else {
            reply.error(ENOENT);
            return;
        };

        // T6-02: `list_children` só roda na primeira chamada desta sessão de
        // `opendir` — chamadas seguintes (o kernel drena diretórios grandes
        // em várias chamadas de `readdir`, retomando do `offset` devolvido)
        // reaproveitam o mesmo vetor já montado, em vez de reconsultar o
        // índice inteiro de novo a cada uma.
        if !self.open_dirs.lock().expect("lock síncrono").contains_key(&fh) {
            let interactive = crate::activity::is_interactive(req.pid(), crate::activity::cached_policy());
            self.core.mark_directory_active(item_id, interactive);

            let children = match self.rt.block_on(self.core.list_children(item_id)) {
                Ok(children) => children,
                Err(err) => {
                    reply.error(Self::errno_for(&err));
                    return;
                }
            };

            let mut entries: Vec<(u64, FileType, String)> =
                vec![(ino, FileType::Directory, ".".to_string()), (ino, FileType::Directory, "..".to_string())];
            for child in &children {
                let inode = self.inode_for(child);
                self.register_inode(inode, child.item_id);
                let kind = match child.kind {
                    ItemKind::Directory => FileType::Directory,
                    ItemKind::File => FileType::RegularFile,
                };
                entries.push((inode, kind, child.name.clone()));
            }
            self.open_dirs.lock().expect("lock síncrono").insert(fh, entries);
        }

        let guard = self.open_dirs.lock().expect("lock síncrono");
        let entries = guard.get(&fh).expect("inserido logo acima se ainda não existia");
        for (i, (inode, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            // `add` retorna `true` quando o buffer do kernel está cheio —
            // a próxima chamada retoma a partir deste `offset` (i + 1).
            if reply.add(*inode, (i + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let Some(item_id) = self.resolve_inode(ino) else {
            reply.error(ENOENT);
            return;
        };
        // O_WRONLY/O_RDWR materializa a cópia dirty (COW) em vez de
        // hidratar somente-leitura — permite `echo >> arquivo_remoto` sem
        // um `write()` anterior que dispare o COW por conta própria.
        let writable = (flags & libc::O_ACCMODE) != libc::O_RDONLY;

        let hydrate_result = if writable {
            self.rt.block_on(self.core.begin_write(item_id))
        } else {
            self.rt.block_on(self.core.open_and_hydrate(item_id))
        };

        match hydrate_result {
            Ok(path) => {
                let file = if writable {
                    std::fs::OpenOptions::new().read(true).write(true).open(&path)
                } else {
                    std::fs::File::open(&path)
                };
                match file {
                    Ok(file) => {
                        // Protege o objeto de cache de eviction enquanto este
                        // handle estiver aberto (SPEC §12.5) — precisa ser
                        // registrado antes de expor o `fh` à aplicação chamadora.
                        if let Err(err) = self.rt.block_on(self.core.mark_handle_opened(item_id)) {
                            tracing::warn!(?err, "falha ao registrar handle aberto");
                        }
                        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                        self.open_files.lock().expect("lock síncrono").insert(fh, OpenFile { item_id, file, writable });
                        reply.opened(fh, 0);
                    }
                    Err(_) => reply.error(EIO),
                }
            }
            Err(SyncError::InvalidOperation(_)) => reply.error(EISDIR),
            Err(err) => {
                tracing::warn!(?err, "open_and_hydrate falhou");
                reply.error(Self::errno_for(&err));
            }
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_item_id) = self.resolve_inode(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(name_str) = name.to_str() else {
            reply.error(ENOENT);
            return;
        };

        let item_id = match self.rt.block_on(self.core.create_local_item(parent_item_id, name_str, ItemKind::File)) {
            Ok(item_id) => item_id,
            Err(err) => {
                reply.error(Self::errno_for(&err));
                return;
            }
        };

        let item = match self.rt.block_on(self.core.get_item(item_id)) {
            Ok(Some(item)) => item,
            _ => {
                reply.error(EIO);
                return;
            }
        };
        let inode = self.inode_for(&item);
        self.register_inode(inode, item_id);

        // `create_local_item` já materializou a cópia dirty vazia — reabrir
        // aqui só recupera o caminho para o `fh`, sem gerar uma segunda
        // geração local (SPEC §16.1, `begin_write` é idempotente).
        let path = match self.rt.block_on(self.core.begin_write(item_id)) {
            Ok(path) => path,
            Err(err) => {
                reply.error(Self::errno_for(&err));
                return;
            }
        };
        let file = match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(_) => {
                reply.error(EIO);
                return;
            }
        };
        if let Err(err) = self.rt.block_on(self.core.mark_handle_opened(item_id)) {
            tracing::warn!(?err, "falha ao registrar handle aberto");
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.open_files.lock().expect("lock síncrono").insert(fh, OpenFile { item_id, file, writable: true });
        reply.created(&ATTR_TTL, &self.file_attr(inode, &item), 0, fh, 0);
    }

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let Some(parent_item_id) = self.resolve_inode(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(name_str) = name.to_str() else {
            reply.error(ENOENT);
            return;
        };

        match self.rt.block_on(self.core.create_local_item(parent_item_id, name_str, ItemKind::Directory)) {
            Ok(item_id) => match self.rt.block_on(self.core.get_item(item_id)) {
                Ok(Some(item)) => {
                    let inode = self.inode_for(&item);
                    self.register_inode(inode, item_id);
                    reply.entry(&ATTR_TTL, &self.file_attr(inode, &item), 0);
                }
                _ => reply.error(EIO),
            },
            Err(err) => reply.error(Self::errno_for(&err)),
        }
    }

    /// FR-FS-005: links simbólicos/físicos e nós de dispositivo/fifo/socket
    /// não são suportados — a árvore remota não tem conceito equivalente.
    fn mknod(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(libc::ENOTSUP);
    }

    fn symlink(&mut self, _req: &Request<'_>, _parent: u64, _link_name: &OsStr, _target: &std::path::Path, reply: ReplyEntry) {
        reply.error(libc::ENOTSUP);
    }

    fn link(&mut self, _req: &Request<'_>, _ino: u64, _newparent: u64, _newname: &OsStr, reply: ReplyEntry) {
        reply.error(libc::ENOTSUP);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove_item(parent, name, ItemKind::File, reply);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove_item(parent, name, ItemKind::Directory, reply);
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let Some(parent_item_id) = self.resolve_inode(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(new_parent_item_id) = self.resolve_inode(newparent) else {
            reply.error(ENOENT);
            return;
        };
        let (Some(name_str), Some(new_name_str)) = (name.to_str(), newname.to_str()) else {
            reply.error(ENOENT);
            return;
        };

        match self.rt.block_on(self.core.rename_local_item(parent_item_id, name_str, new_parent_item_id, new_name_str)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(Self::errno_for(&err)),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        use std::io::{Read, Seek, SeekFrom};

        let mut files = self.open_files.lock().expect("lock síncrono");
        let Some(open_file) = files.get_mut(&fh) else {
            reply.error(libc::EBADF);
            return;
        };
        let file = &mut open_file.file;

        if file.seek(SeekFrom::Start(offset as u64)).is_err() {
            reply.error(EIO);
            return;
        }

        let mut buf = vec![0u8; size as usize];
        match file.read(&mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(_) => reply.error(EIO),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        use std::io::{Seek, SeekFrom, Write};

        let (item_id, new_len) = {
            let mut files = self.open_files.lock().expect("lock síncrono");
            let Some(open_file) = files.get_mut(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            if !open_file.writable {
                reply.error(libc::EBADF);
                return;
            }

            if open_file.file.seek(SeekFrom::Start(offset as u64)).is_err() {
                reply.error(EIO);
                return;
            }
            if open_file.file.write_all(data).is_err() {
                reply.error(EIO);
                return;
            }
            let new_len = match open_file.file.metadata() {
                Ok(meta) => meta.len(),
                Err(_) => {
                    reply.error(EIO);
                    return;
                }
            };
            (open_file.item_id, new_len)
        };

        // Mantém `getattr` reportando o tamanho real enquanto o arquivo
        // segue aberto para escrita, sem esperar `flush`/`release`.
        if let Err(err) = self.rt.block_on(self.core.update_local_size(item_id, new_len)) {
            tracing::warn!(?err, "falha ao atualizar tamanho local após escrita");
        }
        // SPEC §16.2, terceiro gatilho: reagenda o debounce de 5s ociosos a
        // cada escrita — não bloqueia a resposta deste `write()`.
        self.core.schedule_write_idle_stabilization(item_id);
        reply.written(data.len() as u32);
    }

    /// SPEC §16.2: um dos gatilhos de estabilização — chamado a cada
    /// `close()` da aplicação (pode ocorrer mais de uma vez por `open`, via
    /// `dup`/`fork`; `stabilize_upload` é idempotente e tolera isso).
    fn flush(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let item_id = self
            .open_files
            .lock()
            .expect("lock síncrono")
            .get(&fh)
            .filter(|f| f.writable)
            .map(|f| f.item_id);

        if let Some(item_id) = item_id {
            if let Err(err) = self.rt.block_on(self.core.stabilize_upload(item_id)) {
                tracing::warn!(?err, "falha ao estabilizar upload em flush");
            }
        }
        reply.ok();
    }

    /// SPEC §16.2: segundo gatilho de estabilização, além de garantir a
    /// durabilidade local do conteúdo dirty antes de responder.
    fn fsync(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        let item_id = {
            let mut files = self.open_files.lock().expect("lock síncrono");
            let Some(open_file) = files.get_mut(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            let _ = open_file.file.sync_all();
            open_file.writable.then_some(open_file.item_id)
        };

        if let Some(item_id) = item_id {
            if let Err(err) = self.rt.block_on(self.core.stabilize_upload(item_id)) {
                tracing::warn!(?err, "falha ao estabilizar upload em fsync");
            }
        }
        reply.ok();
    }

    /// Terceiro gatilho de estabilização (SPEC §16.2: "`release` do último
    /// handle gravável") — só dispara quando nenhum outro `fh` gravável
    /// para o mesmo item continua aberto (ex.: `dup`/`fork` mantendo uma
    /// cópia do descritor).
    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let removed = self.open_files.lock().expect("lock síncrono").remove(&fh);
        let Some(open_file) = removed else {
            reply.ok();
            return;
        };

        if let Err(err) = self.rt.block_on(self.core.mark_handle_closed(open_file.item_id)) {
            tracing::warn!(?err, "falha ao liberar handle aberto");
        }

        if open_file.writable {
            let still_open_writable = self
                .open_files
                .lock()
                .expect("lock síncrono")
                .values()
                .any(|f| f.item_id == open_file.item_id && f.writable);
            if !still_open_writable {
                if let Err(err) = self.rt.block_on(self.core.stabilize_upload(open_file.item_id)) {
                    tracing::warn!(?err, "falha ao estabilizar upload em release");
                }
            }
        }
        reply.ok();
    }
}
